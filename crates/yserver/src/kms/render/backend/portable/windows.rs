use super::*;

/// #133 step 3 (3.3) — a window's storage extent: the bordered extent
/// `(w + 2bw) x (h + 2bw)`, mirroring Xorg's `compAllocPixmap`
/// (`composite/compalloc.c:610`, `w + (bw << 1)`), with the client
/// content living at `(bw, bw)` inside it. Collapses to exactly
/// `(w, h)` at `bw == 0`.
pub(in crate::kms::render::backend) fn bordered_storage_extent(
    width: u16,
    height: u16,
    border_width: u16,
) -> (u32, u32) {
    let bw2 = u32::from(border_width).saturating_mul(2);
    (
        u32::from(width).saturating_add(bw2).max(1),
        u32::from(height).saturating_add(bw2).max(1),
    )
}

/// #133 step 6 (6.2) — the content copy a border-width change needs:
/// `(src_rect, dst_pos)` in STORAGE coordinates, or `None` when there
/// is nothing to move.
///
/// The source is the OLD content rect, derived entirely from the old
/// allocation (`extent − 2·offset` is the content extent it was
/// allocated for), intersected with the new content extent — Xorg
/// copies the same intersection, since `compCopyWindow` clips the
/// recovered region to `pWin->borderClip`
/// (`composite/compwindow.c:515`). The destination is the new content
/// origin `(bw, bw)`, so a pixel the client drew at content `(cx, cy)`
/// is still at content `(cx, cy)` afterwards — the whole point of the
/// migration.
///
/// `None` when either content rect is empty, which is the only case
/// with nothing to move. Whether a copy is WANTED at all is the
/// caller's decision (`LeafContent`), not this function's.
pub(in crate::kms::render::backend) fn migrated_content_copy(
    old_extent: ash::vk::Extent2D,
    old_offset: i32,
    new_w: u16,
    new_h: u16,
    new_offset: i32,
) -> Option<(ash::vk::Rect2D, ash::vk::Offset2D)> {
    let old_bw2 = u32::try_from(old_offset).unwrap_or(0).saturating_mul(2);
    let old_content_w = old_extent.width.saturating_sub(old_bw2);
    let old_content_h = old_extent.height.saturating_sub(old_bw2);
    let copy_w = old_content_w.min(u32::from(new_w));
    let copy_h = old_content_h.min(u32::from(new_h));
    if copy_w == 0 || copy_h == 0 {
        return None;
    }
    Some((
        ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: old_offset,
                y: old_offset,
            },
            extent: ash::vk::Extent2D {
                width: copy_w,
                height: copy_h,
            },
        },
        ash::vk::Offset2D {
            x: new_offset,
            y: new_offset,
        },
    ))
}

/// #133 step 4 (P5) — the border ring, as up to four disjoint rects:
/// `outer − inner`, i.e. exactly what Xorg paints for `PW_BORDER`
/// (`RegionSubtract(&exposed, &pWin->borderClip, &pWin->winSize)`,
/// `dix/window.c:1586` and `composite/compwindow.c:114`).
///
/// Both rects are in the SAME frame (the window's backing/storage
/// space) and `inner` must be contained in `outer`; the caller builds
/// `outer` by expanding `inner` by the border width. The split is
/// full-width top and bottom bands plus the two side bars between
/// them, which tiles the annulus exactly once — a tiled fill must not
/// paint a corner twice, since `PictOp Src` is idempotent but the
/// damage/submit accounting is not.
///
/// Empty bands are dropped, so `inner == outer` (`bw == 0`) yields an
/// empty vector and no caller ever submits anything.
pub(in crate::kms::render::backend) fn border_ring_rects(
    outer: vk::Rect2D,
    inner: vk::Rect2D,
) -> Vec<vk::Rect2D> {
    let rect = |x: i32, y: i32, w: i32, h: i32| {
        (w > 0 && h > 0).then(|| vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: u32::try_from(w).unwrap_or(0),
                height: u32::try_from(h).unwrap_or(0),
            },
        })
    };
    let ox = outer.offset.x;
    let oy = outer.offset.y;
    let ox1 = ox.saturating_add_unsigned(outer.extent.width);
    let oy1 = oy.saturating_add_unsigned(outer.extent.height);
    let ix = inner.offset.x;
    let iy = inner.offset.y;
    let ix1 = ix.saturating_add_unsigned(inner.extent.width);
    let iy1 = iy.saturating_add_unsigned(inner.extent.height);
    [
        // Top band, full outer width.
        rect(ox, oy, ox1 - ox, iy - oy),
        // Bottom band, full outer width.
        rect(ox, iy1, ox1 - ox, oy1 - iy1),
        // Left bar, between the two bands.
        rect(ox, iy, ix - ox, iy1 - iy),
        // Right bar, between the two bands.
        rect(ix1, iy, ox1 - ix1, iy1 - iy),
    ]
    .into_iter()
    .flatten()
    .collect()
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn alloc_window_stack_rank(&mut self) -> u64 {
        let rank = self.next_window_stack_rank;
        self.next_window_stack_rank = self.next_window_stack_rank.saturating_add(1);
        rank
    }

    /// The UNREDIRECT entry point: re-sync a window's leaf storage to
    /// its current geometry, reallocating at the bordered extent and
    /// initialising the whole allocation from the background, DISCARDING
    /// what was there. That is right HERE and only here: the leaf has
    /// been stale since the route was installed, and the caller
    /// immediately puts the compositor's pixels back with
    /// [`Self::restore_leaves_from_backing`].
    ///
    /// The resize path picks its own [`LeafContent`] in
    /// `configure_subwindow` (#143) and a border-width change migrates
    /// (see [`Self::relayout_window_leaf_storage_for_border_change`]),
    /// so both call [`Self::sync_window_leaf_storage`] directly.
    pub(in crate::kms::render::backend) fn sync_window_leaf_storage_to_geometry(
        &mut self,
        host_xid: u32,
    ) {
        self.sync_window_leaf_storage(host_xid, LeafContent::Discard);
    }

    /// Un-redirecting a window must leave its content where the
    /// compositor had it. Xorg gets that for free: `compSetParentPixmap`
    /// (`composite/compalloc.c:649`) points the window back at the
    /// SCREEN pixmap, which already holds the pixels the compositor
    /// painted, so it copies nothing at all. The copy lives on the
    /// REDIRECT side there — `compNewPixmap` seeds the new backing from
    /// the parent with `CopyArea(…, IncludeInferiors)`
    /// (`composite/compalloc.c:556`) — and the invariant both halves
    /// keep is that a window's pixels stay CONTINUOUS with the screen
    /// across either transition.
    ///
    /// yserver keeps a private per-window leaf, so that continuity has
    /// to be re-established by hand: B holds every paint the client made
    /// while redirected, and W's leaf has been stale since the route was
    /// installed — `process_request.rs` says so while (correctly)
    /// declining the copy in the OTHER direction: "W's storage which
    /// under Manual is empty". Without this restore, the caller has just
    /// re-initialised that leaf from the background, and the window goes
    /// blank the instant a compositor lets go.
    ///
    /// Measured on HW (bee, sonicDE/KWin, 2026-09-11): mpv going
    /// fullscreen sets `_NET_WM_BYPASS_COMPOSITOR`, KWin suspends
    /// compositing screen-wide, and every window blanked to its leaf's
    /// init colour. Covered by
    /// `unredirect_restores_the_window_leaf_from_the_backing`.
    ///
    /// `OP_SRC`, never a blend: this reproduces a `CopyArea`, which
    /// REPLACES the destination and has no notion of alpha. `PictOpOver`
    /// here would make a depth-32 window with α = 0 a no-op and leave
    /// the background showing through — the same trap the
    /// backing-reconstruct walk fell into.
    pub(in crate::kms::render::backend) fn restore_leaves_from_backing(
        &mut self,
        w_xid: u32,
        b_id: DrawableId,
    ) {
        use crate::kms::{
            render::engine::{ResolvedSource, SourceDrawable},
            vk::ops::render::CompositeRect,
        };

        // The SEED plan, read backwards. `plan_backing_inferiors` walks
        // W and its whole mapped subtree and yields, per leaf, that
        // leaf's rect in leaf-local coords (`src`) paired with where it
        // sits in B (`dst`). Seeding copies leaf → B; restoring copies
        // B → leaf, i.e. the identical plan with the two swapped.
        //
        // Walking the SUBTREE is the whole point, and a per-window copy
        // is not enough: a compositor redirects with
        // `RedirectSubwindows(root)`, so the redirected windows are the
        // WM's FRAMES, and the client's own window is reparented inside
        // one — a grandchild, whose pixels live in the frame's B at an
        // offset while its own leaf goes stale. Restoring only the
        // frame's leaf brought the decorations back and left the client
        // window black (measured on HW, bee/sonicDE, 2026-09-11:
        // dolphin still blanked, and repainted in full on hover — the
        // client could rebuild content the server had lost).
        //
        // Any child holding its OWN `redirected_target` is pruned by
        // the planner, which stays right here: its pixels are in its
        // own backing and come back when THAT backing is released.
        let plan = self.plan_backing_inferiors(w_xid, b_id);
        if plan.is_empty() {
            return;
        }
        // Coverage diagnostic. The restore can only hand back what B
        // holds, so a leaf whose storage is TALLER or WIDER than the
        // rect the plan gives it comes back part-painted, the
        // remainder still showing the background re-init — measured on
        // HW as a Plasma panel restored to its top half only, which
        // corrected itself after a few redirect cycles. Log the three
        // extents that decide it rather than guess which one is stale.
        if log::log_enabled!(log::Level::Debug) {
            let b_extent = self.store.get(b_id).map(|d| d.storage.extent);
            for d in &plan {
                let leaf_extent = self.store.get(d.leaf_id).map(|s| s.storage.extent);
                let short = leaf_extent.is_some_and(|e| {
                    let avail_w = e
                        .width
                        .saturating_sub(u32::try_from(d.src_x.max(0)).unwrap_or(0));
                    let avail_h = e
                        .height
                        .saturating_sub(u32::try_from(d.src_y.max(0)).unwrap_or(0));
                    d.width < avail_w || d.height < avail_h
                });
                log::debug!(
                    "render restore_leaves_from_backing: W=0x{w_xid:x} leaf={} \
                     leaf_extent={leaf_extent:?} b_extent={b_extent:?} \
                     rect=src_in_b({},{}) dst_in_leaf({},{}) {}x{}{}",
                    d.leaf_id.as_u64(),
                    d.dst_x,
                    d.dst_y,
                    d.src_x,
                    d.src_y,
                    d.width,
                    d.height,
                    if short { "  SHORT-OF-LEAF" } else { "" },
                );
            }
        }
        // `OP_SRC`, never a blend: this reproduces a `CopyArea`, which
        // REPLACES the destination and has no notion of alpha.
        // `PictOpOver` here would make a depth-32 window with α = 0 a
        // no-op and leave the re-initialised background showing
        // through — the same trap `overlay_backing_inferiors` fell into
        // until 2026-09-11.
        //
        // Occluded regions come back holding the OCCLUDER's pixels,
        // because B only ever held the composited result. That matches
        // Xorg, where an occluded window's pixels are not stored
        // anywhere either (shared screen storage is why X has Expose in
        // the first place); the visible region of every leaf is exact,
        // and uncovering one Exposes it through the normal path.
        const OP_SRC: u8 = 1;
        for d in plan {
            // Skip leaves with no realized view — no GPU storage to
            // copy into. Same liveness check the seed walk makes.
            if self
                .store
                .get(d.leaf_id)
                .is_none_or(|s| s.storage.image_view == ash::vk::ImageView::null())
            {
                continue;
            }
            let rects = [CompositeRect {
                // Inverted against the seed: the plan's backing-local
                // `dst` is this copy's SOURCE, and its leaf-local `src`
                // is this copy's DESTINATION.
                src_x: d.dst_x,
                src_y: d.dst_y,
                mask_x: 0,
                mask_y: 0,
                dst_x: d.src_x,
                dst_y: d.src_y,
                width: d.width,
                height: d.height,
            }];
            match self.engine.render_composite(
                &mut self.store,
                &mut self.platform,
                OP_SRC,
                ResolvedSource::Drawable(SourceDrawable::whole(b_id)),
                ResolvedSource::None,
                Dst::server_internal(d.leaf_id),
                &rects,
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                // Synthesized restore; no Picture context — the engine
                // falls back to the depth heuristic (→ `sample_view`).
                0,
                0,
                0,
            ) {
                Ok(st) if st.recorded_draws > 0 => {
                    self.telemetry.record_paint_submit();
                    self.trace_simple(SubmitKind::RenderComposite, d.leaf_id, st.recorded_draws);
                }
                Ok(_) => {}
                Err(e) => log::warn!(
                    "render restore_leaves_from_backing: copy B→leaf failed for W 0x{w_xid:x}: {e:?}"
                ),
            }
        }
    }

    /// #133 step 6 (P8) — a **border-width** change: reallocate only if
    /// the bordered extent actually moved (6.1), relocate the content
    /// whenever the CONTENT OFFSET moved whether or not it did (6.2),
    /// and damage what the change can have uncovered (6.3).
    ///
    /// This is the ONE place allowed to re-base a window's content
    /// layout, and only by moving the pixels with it: everything else
    /// reads the layout off the allocation
    /// ([`Drawable::content_offset`], [`Self::storage_content_offset`]),
    /// because re-basing without moving pixels displaces everything
    /// already drawn — the step-3 xts5 `Xlib9` `IncludeInferiors`
    /// regression.
    ///
    /// Xorg's shape is `compReallocPixmap` (`composite/compalloc.c:680`):
    /// reallocate iff `w + 2bw` / `h + 2bw` changed, keeping the old
    /// pixmap in `cw->pOldPixmap` "so bits can be recovered"; otherwise
    /// reuse the storage and only update the screen origin. Note its
    /// `else` branch does not move any bytes itself (`:706-712`); in
    /// Xorg the move comes from `ConfigureWindow`, which turns a
    /// border-width change into `MOVE_WIN` (`dix/window.c:2391-2404`),
    /// and `miMoveWindow` then calls `CopyWindow`
    /// (`mi/miwindow.c:293`) with the OLD inside origin — which for a
    /// redirected window lands in `compCopyWindow`'s no-`pOldPixmap`
    /// path (`composite/compwindow.c:542-552`) and falls through to
    /// `fbCopyWindow`, an in-place `miCopyRegion` on one pixmap
    /// (`fb/fbwindow.c:124`).
    ///
    /// **Not** a pure `x`/`y` move (6.4): that changes the window's
    /// screen origin, which the scene walk applies from the geometry,
    /// and relocates nothing storage-local. `configure_subwindow` only
    /// reaches here when `border_width` actually changed.
    ///
    /// Redirected windows are skipped, as on the resize path: their
    /// pixels live in a core-owned backing whose extent is chosen in
    /// `bordered_backing_extent` and rotated by
    /// `rotate_redirected_backing_on_resize`, which also handles a
    /// border-width change after this backend configure returns.
    pub(in crate::kms::render::backend) fn relayout_window_leaf_storage_for_border_change(
        &mut self,
        host_xid: u32,
    ) {
        let Some(old_id) = self.store.lookup(host_xid) else {
            return;
        };
        // 6.3 — damage `old outer ∪ new outer`. Both rects are taken at
        // once by the documented coarse fallback for a configure
        // transition, because the precise pair needs the window's outer
        // origin in SCREEN space and the one helper that computes that
        // (`window_absolute_rect`) accumulates ancestor `x`/`y` without
        // their border widths, so it is only exact when no ancestor is
        // bordered. Re-basing that helper is step 7's business
        // (`window_absolute_position`), so this path takes the superset
        // rather than a rect that could be short by an ancestor's
        // border. A border-width change is rare (awesome sets it once
        // per frame, and recolours rather than resizes on focus), and
        // this is unreachable unless `border_width` actually changed, so
        // no `bw == 0` desktop ever pays for it.
        self.scene.mark_scene_structure_dirty();
        if self.store.redirected_target(old_id).is_none() {
            // Both of `Migrate`'s paths — reallocate-and-copy, and
            // relocate-in-place — repaint the ring themselves, after
            // the content has moved.
            self.sync_window_leaf_storage(host_xid, LeafContent::Migrate);
            return;
        }
        // A redirected window keeps step 4's behaviour: repaint the ring
        // for the CURRENT allocation, whose content offset the backing
        // still owns. Xorg reaches the same place from the other
        // direction — a border-width change marks the window and
        // `miHandleValidateExposures` paints `borderExposed` with
        // `PW_BORDER` (`mi/miwindow.c:216-224`).
        let tile_origin = self.border_tile_origin(host_xid);
        let _ = self.paint_window_border(host_xid, tile_origin);
    }

    pub(in crate::kms::render::backend) fn sync_window_leaf_storage(
        &mut self,
        host_xid: u32,
        content: LeafContent,
    ) {
        let Some(geom) = self.windows.get(&host_xid).copied() else {
            return;
        };
        let Some(old_id) = self.store.lookup(host_xid) else {
            return;
        };
        let new_w = geom.width.max(1);
        let new_h = geom.height.max(1);
        // #133 step 3 (3.3) — storage is the BORDERED extent
        // `(w + 2bw) x (h + 2bw)`, placed at the window's outer origin
        // with content at `(bw, bw)`, mirroring Xorg `compAllocPixmap`
        // (`composite/compalloc.c:610`). At `bw == 0` these are exactly
        // `new_w` / `new_h`, so the compare-and-skip below and the
        // allocation are bit-identical to pre-#133.
        let (storage_w, storage_h) = bordered_storage_extent(new_w, new_h, geom.border_width);
        let new_offset = i32::from(geom.border_width);
        // The OLD layout, read off the allocation: its extent, the
        // content offset it was allocated with, and its depth. The old
        // CONTENT extent is `extent − 2·offset` — both terms are
        // properties of the allocation, so this stays inside step 3's
        // invariant instead of reconstructing the old geometry.
        let old_layout = self
            .store
            .get(old_id)
            .map(|d| (d.storage.extent, d.content_offset, d.depth));
        // #133 step 6 (6.1) — reallocate ONLY if the bordered extent
        // actually changed, exactly as `compReallocPixmap` compares
        // `pix_w != pOld->drawable.width` (`composite/compalloc.c:698`).
        if let Some((old_extent, old_offset, old_depth)) = old_layout
            && old_extent.width == storage_w
            && old_extent.height == storage_h
            && old_depth == geom.depth
        {
            // #133 step 6 (6.2) — the extent survived, but the CONTENT
            // OFFSET may still have moved: `w=100,bw=2 → w=98,bw=3`
            // keeps the outer extent at 104 and moves the content
            // origin from 2 to 3.
            //
            // `Discard` never relocates, by design: re-basing the
            // layout without moving the pixels is exactly what step
            // 3's invariant forbids. Its one caller is unredirect
            // (where the leaf content is being discarded anyway), so
            // the `bw == 0` path returns exactly where it always did.
            // Should unredirect ever find a matching extent with a
            // stale offset, the invariant keeps the window readable at
            // the offset its pixels actually use. A `Migrate` from the
            // resize path lands here only when the bordered extent did
            // not change, and then `old_offset == new_offset` (the
            // border width is what sets both), so it is a no-op too.
            if content == LeafContent::Migrate && old_offset != new_offset {
                self.relocate_leaf_content_in_place(
                    host_xid,
                    old_id,
                    (old_extent, old_offset),
                    (new_w, new_h, new_offset),
                );
            }
            return;
        }

        // Keep the leaf xid stable while replacing the hidden storage.
        // Redirected windows paint through their backing, so resize-time
        // callers can defer this work until unredirect without affecting
        // the compositor-visible pixels.
        //
        // Storage replacement changes the view the walk samples. Both callers
        // (configure, unredirect) wake the scene themselves; this one is here so
        // the walk-skip predicate does not depend on that staying true.
        self.scene.wake_for_damage();
        self.store.detach_xid(host_xid);
        // #133 step 6 (P8) — hold the OLD storage across the
        // reallocation when its content has to survive it. `detach_xid`
        // takes only the xid mapping, leaving the drawable alive in the
        // store at its existing refcount, so `old_id` stays a valid
        // copy SOURCE; the decref moves to after the copy. This is
        // Xorg's `cw->pOldPixmap` retention
        // (`composite/compalloc.c:676-702`).
        let retained_old = match content {
            LeafContent::Discard => {
                self.store_decref_with_invalidate(old_id);
                None
            }
            LeafContent::Migrate => Some(old_id),
        };
        let storage = match self.platform.allocate_drawable_storage_as(
            u16::try_from(storage_w).unwrap_or(u16::MAX),
            u16::try_from(storage_h).unwrap_or(u16::MAX),
            geom.depth,
            crate::kms::vk::mem_accounting::MemCategory::WindowStorage,
        ) {
            Ok(storage) => storage,
            Err(_e) if self.platform.vk.is_none() => {
                crate::kms::render::store::Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: storage_w,
                        height: storage_h,
                    },
                    PlatformBackend::format_for_depth(geom.depth),
                )
            }
            Err(e) => {
                log::warn!(
                    "render sync_window_leaf_storage_to_geometry: alloc storage failed for xid {host_xid:#x}: {e:?}",
                );
                // The window ends up with no storage either way; drop
                // the `pOldPixmap` hold so the old allocation does not
                // leak on the way out.
                if let Some(old_id) = retained_old {
                    self.store_decref_with_invalidate(old_id);
                }
                return;
            }
        };
        if let Err(e) = self.store_alloc(
            host_xid,
            DrawableKind::Window,
            geom.depth,
            geom.mapped,
            storage,
        ) {
            log::warn!(
                "render sync_window_leaf_storage_to_geometry: store.allocate failed for xid {host_xid:#x}: {e:?}",
            );
        } else if let Some(id) = self.store.lookup(host_xid) {
            // #133 step 3 — a re-sync is exactly when the layout may
            // legitimately change: record what this allocation used.
            self.store
                .set_content_offset(id, i32::from(geom.border_width));
            if let Some(bg_pixmap_host_xid) = geom.bg_pixmap {
                // A tiled background covers the CONTENT only, so for a
                // bordered window the ring would stay pool garbage.
                // Pre-fill the whole allocation first (PRIVILEGED). Gated
                // on `bw > 0` so the `bw == 0` path issues exactly the
                // same submits it always did.
                if geom.border_width > 0 {
                    let rect = ash::vk::Rect2D {
                        offset: ash::vk::Offset2D::default(),
                        extent: ash::vk::Extent2D {
                            width: storage_w,
                            height: storage_h,
                        },
                    };
                    let color = geom.bg_pixel.map_or_else(
                        || default_window_init_color(geom.depth),
                        |pixel| {
                            decode_x11_pixel_for_storage(
                                pixel,
                                geom.depth,
                                PlatformBackend::format_for_depth(geom.depth),
                            )
                        },
                    );
                    if let Err(e) = self.engine.fill_rect(
                        &mut self.store,
                        &mut self.platform,
                        Dst::server_internal(id),
                        rect,
                        color,
                    ) {
                        log::debug!(
                            "render sync_window_leaf_storage_to_geometry: ring prefill failed for xid {host_xid:#x}: {e:?}"
                        );
                    }
                }
                if let Err(e) = self.clear_window_area_with_background(
                    host_xid,
                    geom.bg_pixel.unwrap_or(0),
                    Some(bg_pixmap_host_xid),
                    0,
                    0,
                    new_w,
                    new_h,
                    (0, 0),
                ) {
                    log::debug!(
                        "render sync_window_leaf_storage_to_geometry: bg_pixmap init failed for xid {host_xid:#x}: {e:?}"
                    );
                }
            } else {
                let color = geom.bg_pixel.map_or_else(
                    || default_window_init_color(geom.depth),
                    |pixel| {
                        decode_x11_pixel_for_storage(
                            pixel,
                            geom.depth,
                            PlatformBackend::format_for_depth(geom.depth),
                        )
                    },
                );
                let rect = ash::vk::Rect2D {
                    offset: ash::vk::Offset2D::default(),
                    extent: ash::vk::Extent2D {
                        width: storage_w,
                        height: storage_h,
                    },
                };
                // PRIVILEGED backing write: initialising fresh storage
                // covers the whole allocation, ring included, so a
                // bordered window never surfaces the pool returner's
                // pixels in its ring. This is NOT the border paint —
                // the ring gets its real colour in step 4 (P5).
                if let Err(e) = self.engine.fill_rect(
                    &mut self.store,
                    &mut self.platform,
                    Dst::server_internal(id),
                    rect,
                    color,
                ) {
                    log::debug!(
                        "render sync_window_leaf_storage_to_geometry: init fill failed for xid {host_xid:#x}: {e:?}"
                    );
                }
            }
            // #133 step 6 (6.2) — migrate the content the OLD
            // allocation held into the new one, on top of the
            // background init and BEFORE the ring: the copy lands the
            // old content rect on the new content origin, and the ring
            // is then painted around it. Xorg's order is the same —
            // `compCopyWindow` recovers `pOldPixmap`'s bits
            // (`composite/compwindow.c:501-540`) and the border is
            // repainted from `HandleExposures`/`compRepaintBorder`
            // afterwards.
            if let Some(old_id) = retained_old
                && let Some((old_extent, old_offset, _)) = old_layout
            {
                self.migrate_leaf_content_across_realloc(
                    old_id,
                    id,
                    (old_extent, old_offset),
                    (new_w, new_h, new_offset),
                );
            }
            // #133 step 4 (4.4) — GEOMETRY-CHANGE trigger. The
            // allocation above is fresh, and both init paths cover the
            // whole allocation with the BACKGROUND, ring included (so
            // an unpainted ring never shows pool-recycled bytes). The
            // ring's real source goes on top of that, last, exactly as
            // Xorg repaints `borderExposed` after a resize validate
            // (`mi/miwindow.c:216-224`). No-op at `bw == 0`.
            let tile_origin = self.border_tile_origin(host_xid);
            let _ = self.paint_window_border(host_xid, tile_origin);
        }
        // Release the `cw->pOldPixmap` hold. The copy above touched the
        // old storage's render fence, so `decref` parks it in
        // `pending_retire` until the GPU is done with it rather than
        // destroying it under an in-flight op.
        if let Some(old_id) = retained_old {
            self.store_decref_with_invalidate(old_id);
        }
    }

    /// #133 step 6 (6.2) — move a window's content inside ONE storage,
    /// from `old_offset` to the new content offset, when the bordered
    /// extent did not change and so nothing was reallocated.
    ///
    /// The worked case is `w=100,bw=2 → w=98,bw=3`: the outer extent
    /// stays 104, so `compReallocPixmap` keeps the storage — and yet
    /// reinterpreting the bytes in place is wrong twice over. The new
    /// content would sample the old ring along its leading edge, and
    /// the old content's trailing pixels would sit inside the new ring.
    /// Both are fixed by actually copying, and the ring paint that
    /// follows covers everything the copy left outside the new content.
    ///
    /// OVERLAP-SAFE by construction: source and destination are the
    /// same storage, overlapping in all but the offset delta, so this
    /// takes [`RenderEngine::copy_area`]'s `src == dst` path, which
    /// stages the copy through a scratch image rather than reading
    /// texels the same op is writing. Xorg has to be careful in the
    /// same place: `fbCopyWindow` copies within one pixmap through
    /// `miCopyRegion`, whose `careful` flag is set by
    /// `pSrcDrawable == pDstDrawable` (`mi/micopy.c:54`) and reorders
    /// the boxes so an overlapping blit does not eat its own source.
    ///
    /// Both handles are PRIVILEGED: the copy is in backing space and
    /// its source rect is the OLD content rect, which the current
    /// content clip no longer describes.
    fn relocate_leaf_content_in_place(
        &mut self,
        host_xid: u32,
        id: crate::kms::render::store::DrawableId,
        old: (ash::vk::Extent2D, i32),
        new: (u16, u16, i32),
    ) {
        let (old_extent, old_offset) = old;
        let (new_w, new_h, new_offset) = new;
        let copy = migrated_content_copy(old_extent, old_offset, new_w, new_h, new_offset);
        // Record the new layout even if there is nothing to copy: the
        // allocation's content offset is what every reader uses, and
        // the ring paint below takes its thickness from it.
        self.store.set_content_offset(id, new_offset);
        if let Some((src_rect, dst_pos)) = copy
            && let Err(e) = self.engine.copy_area(
                &mut self.store,
                &mut self.platform,
                Src::server_internal(id),
                Dst::server_internal(id),
                src_rect,
                dst_pos,
            )
        {
            log::warn!(
                "render relocate_leaf_content_in_place: content copy failed for xid \
                 {host_xid:#x}: {e:?}"
            );
        }
        // Content the copy did not cover is NEWLY EXPOSED: it held ring
        // pixels before this change (the content can grow while the
        // outer extent stays put — `w=100,bw=3 → w=102,bw=2`). Xorg
        // repaints exactly that region with the window's background
        // from `HandleExposures` → `miPaintWindow(..., PW_BACKGROUND)`
        // (`mi/miexpose.c:445-470`), and leaves it undefined for a
        // window whose background is `None` (miPaintWindow's None
        // early-out), which is why this is gated on having one.
        //
        // AFTER the copy, never before: the copy's SOURCE is the old
        // content rect, which overlaps this region, and both are ops on
        // the SAME storage in the same frame — filling first would have
        // the copy read the fill back.
        if let Some((src_rect, dst_pos)) = copy
            && let Some(geom) = self.windows.get(&host_xid).copied()
            && (geom.bg_pixel.is_some() || geom.bg_pixmap.is_some())
        {
            let covered = ash::vk::Rect2D {
                offset: dst_pos,
                extent: src_rect.extent,
            };
            let new_content = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: new_offset,
                    y: new_offset,
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(new_w),
                    height: u32::from(new_h),
                },
            };
            // `new_content − covered`, as up to four disjoint rects —
            // the same annulus split the ring uses, since `covered`
            // shares the new content's top-left corner.
            for r in border_ring_rects(new_content, covered) {
                let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
                    i16::try_from(r.offset.x - new_offset),
                    i16::try_from(r.offset.y - new_offset),
                    u16::try_from(r.extent.width),
                    u16::try_from(r.extent.height),
                ) else {
                    continue;
                };
                if let Err(e) = self.clear_window_area_with_background(
                    host_xid,
                    geom.bg_pixel.unwrap_or(0),
                    geom.bg_pixmap,
                    x,
                    y,
                    w,
                    h,
                    (0, 0),
                ) {
                    log::debug!(
                        "render relocate_leaf_content_in_place: exposure fill failed for xid \
                         {host_xid:#x}: {e:?}"
                    );
                }
            }
        }
        // The ring's thickness comes from the allocation
        // (`border_ring_thickness`), which the `set_content_offset`
        // above has just made current, so the ring now lands where the
        // relocated content is not.
        let tile_origin = self.border_tile_origin(host_xid);
        let _ = self.paint_window_border(host_xid, tile_origin);
        self.scene.wake_for_damage();
    }

    /// #133 step 6 (6.2) — the same migration across a REALLOCATION:
    /// copy the intersection of the old and new content rects out of
    /// the retained old storage and into the fresh one, from
    /// `old_offset` to `new_offset`.
    ///
    /// Distinct storages, so no overlap to handle; both handles are
    /// PRIVILEGED for the same reason as
    /// [`Self::relocate_leaf_content_in_place`]'s.
    fn migrate_leaf_content_across_realloc(
        &mut self,
        old_id: crate::kms::render::store::DrawableId,
        new_id: crate::kms::render::store::DrawableId,
        old: (ash::vk::Extent2D, i32),
        new: (u16, u16, i32),
    ) {
        let (old_extent, old_offset) = old;
        let (new_w, new_h, new_offset) = new;
        let Some((src_rect, dst_pos)) =
            migrated_content_copy(old_extent, old_offset, new_w, new_h, new_offset)
        else {
            return;
        };
        if let Err(e) = self.engine.copy_area(
            &mut self.store,
            &mut self.platform,
            Src::server_internal(old_id),
            Dst::server_internal(new_id),
            src_rect,
            dst_pos,
        ) {
            log::warn!("render migrate_leaf_content_across_realloc: content copy failed: {e:?}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::kms::render::backend) fn clear_window_area_with_background(
        &mut self,
        host_xid: u32,
        background_pixel: u32,
        background_pixmap_host_xid: Option<u32>,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        tile_origin: (i32, i32),
    ) -> io::Result<()> {
        use crate::kms::{
            render::engine::{ResolvedSource, SourceDrawable},
            vk::ops::render::CompositeRect,
        };

        self.clear_window_area_calls = self.clear_window_area_calls.wrapping_add(1);
        let Some(dst_target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        if let Some(bg_host_xid) = background_pixmap_host_xid
            && let Some(src) = self.store.lookup(bg_host_xid)
        {
            if dst_target.backing_id() == src {
                return Ok(());
            }
            if self.store.get(src).map(|d| d.storage.format)
                == Some(ash::vk::Format::B8G8R8A8_UNORM)
            {
                // Higher-stacked sibling windows that share this backing
                // must not be overwritten by this clear. xfwm4 frames a
                // top-level with a titlebar window (e.g. 1136x28) and the
                // decoration buttons as SIBLINGS that all flatten into the
                // one redirect backing, with the buttons stacked ABOVE the
                // titlebar. Painted in client-request order, the titlebar's
                // background-pixmap clear lands AFTER the buttons and — with
                // no sibling clip — stomps them flat (decoration buttons
                // "briefly show then vanish; reappear on hover"). copy_area
                // already guards this exact shared-backing case via
                // `copy_area_shared_backing_occluders`; mirror it here so
                // ClearArea / background tiling honours the same clip.
                // Coords are dst-window-local (the occluder rects' space);
                // `dst_target.offset()` shifts each surviving piece into the
                // backing.
                let clear_rect = ash::vk::Rect2D {
                    offset: ash::vk::Offset2D {
                        x: i32::from(x),
                        y: i32::from(y),
                    },
                    extent: ash::vk::Extent2D {
                        width: u32::from(width),
                        height: u32::from(height),
                    },
                };
                let surviving: Vec<ash::vk::Rect2D> = if self.windows.contains_key(&host_xid) {
                    let occ = self.copy_area_shared_backing_occluders(host_xid, &dst_target);
                    if occ.is_empty() {
                        vec![clear_rect]
                    } else {
                        compute_copy_area_dst_rects(clear_rect, &occ)
                    }
                } else {
                    vec![clear_rect]
                };
                if surviving.is_empty() {
                    // Fully occluded by higher siblings — nothing to clear.
                    return Ok(());
                }
                let rects: Vec<CompositeRect> = surviving
                    .into_iter()
                    .map(|r| CompositeRect {
                        // ParentRelative tiles sample at the OWNING
                        // window's alignment (tile_origin offset).
                        src_x: r.offset.x + tile_origin.0,
                        src_y: r.offset.y + tile_origin.1,
                        mask_x: 0,
                        mask_y: 0,
                        dst_x: dst_target.offset().0 + r.offset.x,
                        dst_y: dst_target.offset().1 + r.offset.y,
                        width: r.extent.width,
                        height: r.extent.height,
                    })
                    .collect();
                const OP_SRC: u8 = 1;
                let composite_result = self.engine.render_composite(
                    &mut self.store,
                    &mut self.platform,
                    OP_SRC,
                    ResolvedSource::Drawable(SourceDrawable::whole(src)),
                    ResolvedSource::None,
                    dst_target.dst(),
                    &rects,
                    None,
                    Repeat::Normal,
                    Repeat::None,
                    None,
                    None,
                    false,
                    // Audit #4: no Picture context — pass 0 so the
                    // engine falls back to the depth-based swizzle.
                    0,
                    0,
                    0,
                );
                self.sync_descriptor_pool_telemetry();
                match composite_result {
                    Ok(s) if s.recorded_draws > 0 && !s.deferred_to_batch => {
                        self.telemetry.record_paint_submit();
                        self.trace_render(
                            SubmitKind::RenderComposite,
                            dst_target.backing_id(),
                            s.recorded_draws,
                            OP_SRC,
                            SrcClass::Direct,
                            None,
                            SubmitFlags {
                                readback: s.used_dst_readback,
                                alias: s.used_src_alias_scratch,
                                zero_draws: false,
                                upload: false,
                            },
                        );
                        return Ok(());
                    }
                    Ok(_) => return Ok(()),
                    Err(e) => {
                        log::warn!(
                            "render clear_window_area_with_background: tiled bg_pixmap clear failed \
                             for 0x{host_xid:x}: {e:?}"
                        );
                    }
                }
            }
        }
        let rect = Rectangle16 {
            x,
            y,
            width,
            height,
        };
        self.fill_window_background_solid(host_xid, dst_target, background_pixel, &[rect]);
        Ok(())
    }

    /// Paint `rects` (window-local) of `host_xid` in its solid
    /// background `pixel` with the server's own state, never the last
    /// client GC's: Xorg's `miPaintWindow` (`mi/miexpose.c:475-524`)
    /// fills through a scratch GC set to GXcopy, all planes, FillSolid,
    /// clipped to the window's clipList — its mapped children out.
    fn fill_window_background_solid(
        &mut self,
        host_xid: u32,
        target: PaintTarget,
        pixel: u32,
        rects: &[Rectangle16],
    ) {
        use yserver_core::backend::{GcFunction, SubwindowMode};
        let rects = match self.subwindow_mode_clip(host_xid, SubwindowMode::ClipByChildren) {
            Some(clip) => apply_subwindow_mode_clip(&clip, rects),
            None => rects.to_vec(),
        };
        // The root is a depth-24 window kept in depth-32 storage: its
        // background is opaque, as `init_root_storage` fills it.
        let target =
            if self.store.get(target.backing_id()).map(|d| d.kind) == Some(DrawableKind::Root) {
                target.with_x11_depth(24)
            } else {
                target
            };
        self.fill_solid_rects_with(target, pixel, &rects, GcFunction::Copy, u32::MAX);
    }

    /// #133 step 4 (P5) — paint the border ring. PRIVILEGED,
    /// server-internal, in BACKING space.
    ///
    /// The sibling of [`Self::clear_window_area_with_background`]: same
    /// pixel-or-tile choice, border source instead of background
    /// source, the ring instead of a cleared area. It receives exactly
    /// `outer − content` and reaches the storage through
    /// [`PaintTarget::server_backing_dst`], so the content clip step 3
    /// installed on every client route cannot narrow it — and, by the
    /// same construction, no client route can ever paint here.
    ///
    /// Xorg's equivalent is `miPaintWindow(..., PW_BORDER)`
    /// (`mi/miexpose.c:445-470`): it paints into the window's OWN
    /// pixmap (`GetWindowPixmap`), picks solid-vs-tiled from
    /// `pWin->borderIsPixel`, and fills the region its callers compute
    /// as `borderClip − winSize` (`dix/window.c:1586`,
    /// `composite/compwindow.c:114`).
    ///
    /// `tile_origin` has the same meaning as
    /// [`Self::clear_window_area_with_background`]'s: the distance from
    /// this window's content origin to the origin the tile is aligned
    /// to, added to content-local coordinates when sampling. See
    /// [`Self::border_tile_origin`] for how Xorg derives it and what we
    /// can and cannot supply here.
    ///
    /// Not gated on viewability, unlike Xorg's `pWin->viewable` check
    /// at `dix/window.c:1586`: the ring lives inside the window's own
    /// storage, which survives unmap/remap, so painting it whenever the
    /// source or the layout changes is what makes it correct at the
    /// next map. Xorg has to defer because its border pixels live in
    /// the screen pixmap, where an unmapped window owns nothing.
    pub(in crate::kms::render::backend) fn paint_window_border(
        &mut self,
        host_xid: u32,
        tile_origin: (i32, i32),
    ) -> io::Result<()> {
        let Some(geom) = self.windows.get(&host_xid).copied() else {
            return Ok(());
        };
        // `bw == 0` IDENTITY: no resolve, no rects, no submit — the
        // whole existing user base is this path (`HasBorder(pWin)`,
        // `dix/window.c:1586`, is Xorg's same early-out).
        if geom.border_width == 0 {
            return Ok(());
        }
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        let b = self.border_ring_thickness(host_xid, &target);
        if b <= 0 {
            // The storage still has an UNBORDERED layout, and an
            // unbordered layout has no ring: painting one from the new
            // `border_width` would write over content. Step 6 makes
            // `configure_subwindow` migrate the allocation before it
            // gets here, so on that path `b` is the new border width by
            // the time this runs; a window whose allocation could not
            // follow (a redirect backing, a failed allocation) still
            // takes this early-out rather than painting into content.
            return Ok(());
        }
        let Some(storage_extent) = self
            .store
            .get(target.backing_id())
            .map(|d| d.storage.extent)
        else {
            return Ok(());
        };
        let (cx, cy) = target.offset();
        let inner = vk::Rect2D {
            offset: vk::Offset2D { x: cx, y: cy },
            extent: vk::Extent2D {
                width: u32::from(geom.width.max(1)),
                height: u32::from(geom.height.max(1)),
            },
        };
        let outer = vk::Rect2D {
            offset: vk::Offset2D {
                x: cx - b,
                y: cy - b,
            },
            extent: vk::Extent2D {
                width: inner
                    .extent
                    .width
                    .saturating_add(2 * u32::try_from(b).unwrap_or(0)),
                height: inner
                    .extent
                    .height
                    .saturating_add(2 * u32::try_from(b).unwrap_or(0)),
            },
        };
        // Clamp per ring rect, not on `outer` before the subtract: a
        // descendant painting into a redirected ancestor's backing can
        // have part of its ring off that backing, and clamping the
        // annulus's bounding box first would move the ring's edges.
        let rects: Vec<vk::Rect2D> = border_ring_rects(outer, inner)
            .into_iter()
            .map(|r| crate::kms::render::engine::clamp_rect(r, storage_extent))
            .filter(|r| r.extent.width != 0 && r.extent.height != 0)
            .collect();
        if rects.is_empty() {
            return Ok(());
        }
        // `PixUnion border` + `borderIsPixel` is an either/or
        // (`include/windowstr.h:146`), and both the CWA mirror and
        // core's forward preserve that, so the tile is simply tried
        // first and the pixel is the remaining case.
        if let Some(tile_xid) = geom.border_pixmap
            && self.paint_border_ring_tiled(
                host_xid,
                tile_xid,
                &target,
                &rects,
                (cx, cy),
                tile_origin,
            )
        {
            self.scene.wake_for_damage();
            return Ok(());
        }
        let pixel = self.border_solid_pixel(geom, target.backing_id());
        let format = self.store.get(target.backing_id()).map_or_else(
            || PlatformBackend::format_for_depth(geom.depth),
            |d| d.storage.format,
        );
        let depth = self
            .store
            .get(target.backing_id())
            .map_or(geom.depth, |d| d.depth);
        let color = decode_x11_pixel_for_storage(pixel, depth, format);
        if let Err(e) = self.engine.fill_rect_batch(
            &mut self.store,
            &mut self.platform,
            target.server_backing_dst(),
            color,
            &rects,
        ) {
            log::debug!(
                "render paint_window_border: solid ring fill failed for 0x{host_xid:x}: {e:?}"
            );
            return Ok(());
        }
        self.telemetry.record_paint_submit();
        self.scene.wake_for_damage();
        Ok(())
    }

    /// The ring's thickness in the frame `target` resolved to.
    ///
    /// Mirrors exactly what [`Self::resolve_window_paint_target`] used
    /// to place the content: when the window paints into storage it
    /// OWNS (its own leaf storage, or its own redirect backing) the
    /// value is the one that storage was ALLOCATED with
    /// ([`Self::storage_content_offset`]) — the step-3 invariant that
    /// the content layout is a property of the allocation, never of the
    /// live `border_width`. When it paints into an ancestor's backing
    /// the walk seeds `own_bw` from the live geometry instead, so the
    /// ring must use the same value or it would not sit against the
    /// content.
    pub(in crate::kms::render::backend) fn border_ring_thickness(
        &self,
        host_xid: u32,
        target: &PaintTarget,
    ) -> i32 {
        let own = self.store.lookup(host_xid);
        let owns_target = own == Some(target.backing_id())
            || own.and_then(|id| self.store.redirected_target(id)) == Some(target.backing_id());
        if owns_target {
            self.storage_content_offset(host_xid, target.backing_id())
        } else {
            self.window_border_width(host_xid)
        }
    }

    /// The solid border pixel, with Xorg's depth-32 alpha rule applied.
    ///
    /// `mi/miexpose.c:491-511`, under `#ifdef COMPOSITE`, comment
    /// verbatim: "Make sure alpha will sample as 1.0 for opaque
    /// windows". For a depth-32 *drawable* (the window's pixmap) whose
    /// window is itself depth 32, Xorg walks up the parent chain and if
    /// it meets a depth-24 ancestor the effective depth is 24, so
    /// `fill.pixel |= 0xff000000`. The loop is
    /// `while (orig_pWin && orig_pWin->parent)`, i.e. it starts at the
    /// parent and stops BEFORE the root window, whose own depth is
    /// therefore never consulted — our `windows` map does not track the
    /// root at all, so walking `parent` until it leaves the map is the
    /// same traversal.
    ///
    /// The two depths are DIFFERENT tests, and the parent walk only
    /// runs when they agree: the gate is the *pixmap's* depth, while
    /// `effective_depth` starts from the *window's*. So a depth-24
    /// window painting into a depth-32 backing — a child of a
    /// redirected depth-32 frame — takes the alpha unconditionally,
    /// with no walk at all. That case is the one where getting it wrong
    /// is visible: the storage is depth 32, so
    /// `decode_x11_pixel_for_storage` reads alpha out of the pixel, and
    /// the usual `0x00RRGGBB` border literal would leave a fully
    /// TRANSPARENT ring under `alpha_passthrough` compositing.
    ///
    /// A window whose storage depth matches its own needs nothing here:
    /// `decode_x11_pixel_for_storage` forces α = 1.0 for any depth
    /// other than 32 (the L1 server-α invariant,
    /// `render/engine.rs:12014`), which is what the ring fill's
    /// `vkCmdClearAttachments` writes.
    pub(in crate::kms::render::backend) fn border_solid_pixel(
        &self,
        geom: WindowGeometry,
        backing: crate::kms::render::store::DrawableId,
    ) -> u32 {
        let pixel = geom.border_pixel.unwrap_or(0);
        // `if (drawable->depth == 32)` — the DESTINATION pixmap.
        if self.store.get(backing).map_or(geom.depth, |d| d.depth) != 32 {
            return pixel;
        }
        // `int effective_depth = orig_pWin->drawable.depth;`
        let mut effective_depth = geom.depth;
        if effective_depth == 32 {
            let mut cursor = geom.parent;
            while let Some(parent_xid) = cursor {
                let Some(parent) = self.windows.get(&parent_xid) else {
                    // Left the tracked tree — that is the root, which
                    // Xorg's `while (orig_pWin && orig_pWin->parent)`
                    // also declines to examine.
                    break;
                };
                if parent.depth == 24 {
                    effective_depth = 24;
                    break;
                }
                cursor = parent.parent;
            }
        }
        if effective_depth == 24 {
            return pixel | 0xff00_0000;
        }
        pixel
    }

    /// Xorg's tile origin for `PW_BORDER` (`mi/miexpose.c:458-469`):
    ///
    /// ```c
    /// while (pWin->backgroundState == ParentRelative)
    ///     pWin = pWin->parent;
    /// tile_x_off = pWin->drawable.x;
    /// tile_y_off = pWin->drawable.y;
    /// ...
    /// draw_x_off = pixmap->screen_x;
    /// tile_x_off -= draw_x_off;
    /// ```
    ///
    /// Three things fall out of that, and only the first is what the
    /// first draft of the spec said:
    ///
    /// 1. It is NOT unconditionally the window's inner origin. The walk
    ///    is driven by the window's **background** state — even for a
    ///    border — so a ParentRelative-background window aligns its
    ///    border tile to the ancestor the background resolves to.
    /// 2. In the common case (background not ParentRelative) `pWin` is
    ///    the window itself, and since `pixmap->screen_x` is the OUTER
    ///    origin (`drawable.x - bw`, `composite/compalloc.c:610`), the
    ///    GC tile origin comes out as exactly `bw` — the content
    ///    origin. So sampling at `content_local` is right there.
    /// 3. Generally, `tile_x_off = win.drawable.x - ancestor.drawable.x`
    ///    relative to the content origin, which is precisely the
    ///    accumulation `Resources::window_resolved_background` already
    ///    performs for the background tile
    ///    (`resources.rs:1847`, `x + border_width` per level). The
    ///    border tile and the background tile are aligned identically.
    ///
    /// So the ring fill takes the same `tile_origin` the background
    /// clear does, and this returns the value the render side can
    /// actually derive: `(0, 0)`, correct whenever the window's
    /// background is not ParentRelative.
    ///
    /// **Known gap, shared with the background path:** the
    /// ParentRelative accumulation is computed in core
    /// (`resources.rs:1847`) and only reaches the backend on the
    /// `clear_area` route. `change_subwindow_attributes` receives the
    /// already-resolved background, so the render side cannot tell a
    /// ParentRelative window from a concrete one, and
    /// `paint_window_background_rect` / `map_subwindow` pass `(0, 0)`
    /// for the same reason. Closing it is one plumbing change that
    /// fixes both tiles at once; it is not step 4's to make.
    pub(in crate::kms::render::backend) fn border_tile_origin(&self, _host_xid: u32) -> (i32, i32) {
        (0, 0)
    }

    /// The tiled half of [`Self::paint_window_border`]. Returns false
    /// when the tile cannot be sampled, so the caller falls back to the
    /// solid pixel rather than leaving the ring unpainted.
    fn paint_border_ring_tiled(
        &mut self,
        host_xid: u32,
        tile_xid: u32,
        target: &PaintTarget,
        rects: &[vk::Rect2D],
        content_origin: (i32, i32),
        tile_origin: (i32, i32),
    ) -> bool {
        use crate::kms::{
            render::engine::{ResolvedSource, SourceDrawable},
            vk::ops::render::CompositeRect,
        };

        let Some(tile_id) = self.store.lookup(tile_xid) else {
            log::debug!(
                "render paint_border_ring_tiled: border tile 0x{tile_xid:x} not in store for \
                 0x{host_xid:x}"
            );
            return false;
        };
        if target.backing_id() == tile_id {
            // Self-tile would alias src and dst inside render_composite.
            return false;
        }
        let tile_format = self.store.get(tile_id).map(|d| d.storage.format);
        if tile_format != Some(vk::Format::B8G8R8A8_UNORM) {
            log::debug!(
                "render paint_border_ring_tiled: tile 0x{tile_xid:x} format {tile_format:?} not \
                 BGRA8"
            );
            return false;
        }
        // The rects are in BACKING space; the tile phase is defined in
        // content-local space (`miexpose.c:461` after the `screen_x`
        // subtraction — see `border_tile_origin`), so shift by the
        // content origin and add the ParentRelative accumulation.
        // Coordinates in the ring are NEGATIVE on the top and left
        // sides, which `Repeat::Normal` handles: the shader wraps with
        // `uv - floor(uv)` (`shaders/render.frag.glsl:97`), correct for
        // negative uv.
        let composite_rects: Vec<CompositeRect> = rects
            .iter()
            .map(|r| CompositeRect {
                src_x: r.offset.x - content_origin.0 + tile_origin.0,
                src_y: r.offset.y - content_origin.1 + tile_origin.1,
                mask_x: 0,
                mask_y: 0,
                dst_x: r.offset.x,
                dst_y: r.offset.y,
                width: r.extent.width,
                height: r.extent.height,
            })
            .collect();
        // PictOp Src — the border tile replaces whatever the ring holds.
        const OP_SRC: u8 = 1;
        let composite_result = self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            OP_SRC,
            // Border pixmaps are pixmaps by protocol — whole storage.
            ResolvedSource::Drawable(SourceDrawable::whole(tile_id)),
            ResolvedSource::None,
            // PRIVILEGED: the ring is outside the content clip by
            // definition, so the client-facing `target.dst()` would
            // scissor every rect away to nothing.
            target.server_backing_dst(),
            &composite_rects,
            None,
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
            // No Picture context — the engine falls back to the
            // depth-based swizzle, same as the background tile clear.
            0,
            0,
            0,
        );
        self.sync_descriptor_pool_telemetry();
        match composite_result {
            Ok(s) => {
                if s.recorded_draws > 0 && !s.deferred_to_batch {
                    self.telemetry.record_paint_submit();
                    self.trace_render(
                        SubmitKind::RenderComposite,
                        target.backing_id(),
                        s.recorded_draws,
                        OP_SRC,
                        SrcClass::Direct,
                        None,
                        SubmitFlags {
                            readback: s.used_dst_readback,
                            alias: s.used_src_alias_scratch,
                            zero_draws: false,
                            upload: false,
                        },
                    );
                }
                true
            }
            Err(e) => {
                log::warn!(
                    "render paint_border_ring_tiled: render_composite failed for 0x{host_xid:x}: \
                     {e:?}"
                );
                false
            }
        }
    }

    pub(in crate::kms::render::backend) fn restack_subwindow(
        &mut self,
        host_xid: u32,
        stack_mode: u8,
        sibling: Option<u32>,
    ) {
        let Some(current) = self.windows.get(&host_xid).copied() else {
            return;
        };
        let parent = current.parent;
        let mut siblings: Vec<(u32, u64)> = self
            .windows
            .iter()
            .filter_map(|(xid, geom)| (geom.parent == parent).then_some((*xid, geom.stack_rank)))
            .collect();
        siblings.sort_by_key(|(_, rank)| *rank);
        let Some(pos) = siblings.iter().position(|(xid, _)| *xid == host_xid) else {
            return;
        };
        let entry = siblings.remove(pos);
        let sibling_pos = sibling.and_then(|sib| siblings.iter().position(|(xid, _)| *xid == sib));
        match stack_mode {
            0 | 2 | 4 => match sibling_pos {
                Some(sp) => siblings.insert(sp + 1, entry),
                None => siblings.push(entry),
            },
            1 | 3 => match sibling_pos {
                Some(sp) => siblings.insert(sp, entry),
                None => siblings.insert(0, entry),
            },
            _ => siblings.push(entry),
        }
        for (rank, (xid, _)) in siblings.into_iter().enumerate() {
            if let Some(geom) = self.windows.get_mut(&xid) {
                geom.stack_rank = u64::try_from(rank).unwrap_or(u64::MAX);
            }
        }
    }

    /// Tile `host_pixmap_xid` across the whole root extent from (0, 0): the
    /// root's background pixmap, painted by `set_container_background_pixmap`
    /// and again whenever the root storage is reallocated for a new screen
    /// size (Xorg `SetRootClip` exposes the whole resized root and
    /// `miPaintWindow` tiles its background there).
    pub(in crate::kms::render::backend) fn tile_root_background_pixmap(
        &mut self,
        host_pixmap_xid: u32,
    ) {
        use crate::kms::{
            render::engine::{ResolvedSource, SourceDrawable},
            vk::ops::render::CompositeRect,
        };
        // Stage 4a — root paint resolves through redirect routing.
        let Some(dst_target) = self.resolve_paint_target(self.core.window_id) else {
            return;
        };
        let dst = dst_target.backing_id();
        let Some(src) = self.store.lookup(host_pixmap_xid) else {
            log::debug!(
                "render set_container_background_pixmap: pixmap 0x{host_pixmap_xid:x} not in store"
            );
            return;
        };
        // Stage 3f.14: X11 bg_pixmap tiles across the drawable
        // extent. Pre-3f.14 v2 did a single copy_area at (0, 0)
        // and left the rest of root unchanged — fvwm3 wallpaper
        // covered only the top-left of the screen on bee. Route
        // through `engine.render_composite` with OP_SRC + Repeat::
        // Normal so the source pixmap tiles across the whole root
        // extent in a single submit. Same shape as `try_tiled_fill`
        // (3f.3) but unconditioned by GC clip.
        if src == dst {
            // Defensive: a pixmap aliased as bg of its own drawable
            // is not a meaningful X11 op. v1's path treats it the
            // same (copy_area with src == dst is logged + skipped).
            log::debug!("render set_container_background_pixmap: src == root, skipping");
            return;
        }
        let src_format = self.store.get(src).map(|d| d.storage.format);
        if src_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            // Tile path requires BGRA8 src (matches `try_tiled_fill`
            // gate). Other formats fall through with no paint —
            // v1-parity-ish; rare in practice for root bg.
            log::debug!(
                "render set_container_background_pixmap: pixmap 0x{host_pixmap_xid:x} format \
                 {src_format:?} not BGRA8, skipping tile"
            );
            return;
        }
        let dst_extent = ash::vk::Extent2D {
            width: u32::from(self.platform.fb_w.max(1)),
            height: u32::from(self.platform.fb_h.max(1)),
        };
        let rects = [CompositeRect {
            src_x: 0,
            src_y: 0,
            mask_x: 0,
            mask_y: 0,
            dst_x: dst_target.offset().0,
            dst_y: dst_target.offset().1,
            width: dst_extent.width,
            height: dst_extent.height,
        }];
        const OP_SRC: u8 = 1;
        let composite_result = self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            OP_SRC,
            ResolvedSource::Drawable(SourceDrawable::whole(src)),
            ResolvedSource::None,
            dst_target.dst(),
            &rects,
            None,
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
            // Audit #4: synthesized backing-seed copy, no Picture
            // context. Engine falls back to depth heuristic.
            0,
            0,
            0,
        );
        self.sync_descriptor_pool_telemetry();
        match composite_result {
            Ok(s) if s.recorded_draws > 0 && !s.deferred_to_batch => {
                self.telemetry.record_paint_submit();
                self.trace_render(
                    SubmitKind::RenderComposite,
                    dst,
                    s.recorded_draws,
                    1, // OP_SRC
                    SrcClass::Direct,
                    None,
                    SubmitFlags {
                        readback: s.used_dst_readback,
                        alias: s.used_src_alias_scratch,
                        zero_draws: false,
                        upload: false,
                    },
                );
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!(
                    "render set_container_background_pixmap: render_composite tile failed: {e:?}"
                );
            }
        }
    }

    /// Record a window's geometry, parent and background, unviewable and without storage.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::kms::render::backend) fn register_window_geometry(
        &mut self,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        depth: u8,
        parent: Option<u32>,
        bg_pixel: Option<u32>,
    ) {
        if self.windows.contains_key(&host_xid) {
            return;
        }
        let stack_rank = self.alloc_window_stack_rank();
        self.windows.insert(
            host_xid,
            WindowGeometry {
                border_width,
                border_pixel: None,
                border_pixmap: None,
                x,
                y,
                width,
                height,
                depth,
                mapped: false,
                viewable: false,
                parent,
                stack_rank,
                bg_pixel,
                bg_pixmap: None,
                cursor: None,
            },
        );
    }

    /// Allocate and clear a window's leaf at its bordered extent; returns an existing leaf as is.
    pub(in crate::kms::render::backend) fn allocate_window_leaf(
        &mut self,
        host_xid: u32,
    ) -> Option<DrawableId> {
        if let Some(id) = self.store.lookup(host_xid) {
            return Some(id);
        }
        let geom = self.windows.get(&host_xid).copied()?;
        // #133 step 3: the bordered extent, as Xorg `compAllocPixmap` (`compalloc.c:610`).
        let (storage_w, storage_h) =
            bordered_storage_extent(geom.width.max(1), geom.height.max(1), geom.border_width);
        let storage = match self.platform.allocate_drawable_storage_as(
            u16::try_from(storage_w).unwrap_or(u16::MAX),
            u16::try_from(storage_h).unwrap_or(u16::MAX),
            geom.depth,
            crate::kms::vk::mem_accounting::MemCategory::WindowStorage,
        ) {
            Ok(storage) => storage,
            Err(e)
                if self.platform.vk.is_none()
                    && e == ash::vk::Result::ERROR_INITIALIZATION_FAILED =>
            {
                // No Vk fixture (`for_tests`): track the geometry without storage.
                log::debug!("render allocate_window_leaf: no Vk for xid {host_xid:#x}: {e:?}");
                return None;
            }
            Err(e) => {
                log::warn!(
                    "render allocate_window_leaf: allocation failed for xid {host_xid:#x} \
                     {}x{} d{}: {e:?}",
                    geom.width,
                    geom.height,
                    geom.depth,
                );
                return None;
            }
        };
        if let Err(e) = self.store_alloc(
            host_xid,
            DrawableKind::Window,
            geom.depth,
            geom.mapped,
            storage,
        ) {
            log::warn!(
                "render allocate_window_leaf: store.allocate failed for xid {host_xid:#x}: {e:?}",
            );
            return None;
        }
        let id = self.store.lookup(host_xid)?;
        // #133 step 3 — the layout this storage was allocated with, for `storage_content_offset`.
        self.store
            .set_content_offset(id, i32::from(geom.border_width));
        self.telemetry.record_storage_allocation();
        self.telemetry.record_image_view_create();
        let format = PlatformBackend::format_for_depth(geom.depth);
        let color = geom.bg_pixel.map_or_else(
            || default_window_init_color(geom.depth),
            |pixel| decode_x11_pixel_for_storage(pixel, geom.depth, format),
        );
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: storage_w,
                height: storage_h,
            },
        };
        // PRIVILEGED backing write: the initial clear covers the whole allocation, ring included.
        if let Err(e) = self.engine.fill_rect(
            &mut self.store,
            &mut self.platform,
            Dst::server_internal(id),
            rect,
            color,
        ) {
            log::debug!(
                "render allocate_window_leaf: initial fill failed for xid {host_xid:#x}: {e:?}"
            );
        }
        Some(id)
    }

    /// True for the two windows whose storage is outside the viewability lifecycle.
    pub(in crate::kms::render::backend) fn storage_lifecycle_exempt(&self, host_xid: u32) -> bool {
        host_xid == self.core.window_id
            || host_xid == yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0
    }

    /// A paint op found no target: a known window is hidden (clipped away), anything else a gap.
    pub(in crate::kms::render::backend) fn log_unresolved_target(
        &self,
        host_xid: u32,
        method: &'static str,
    ) {
        if self.windows.contains_key(&host_xid) {
            log::trace!("render {method}: window 0x{host_xid:x} is not viewable; clipped away");
        } else {
            self.log_render_gap(method);
        }
    }
}

/// Map a host-visual descriptor to a depth for the storage
/// allocator. Stage 2d picks BGRA32 for `CopyFromParent` (the
/// default visual is depth-24 ARGB-equivalent in our advertised
/// pixel format) and honours an explicit depth otherwise.
/// Stage 3c: walk a `PictureRecord` and resolve it into the
/// engine's `ResolvedSource` plus the per-picture sampler attrs
/// (`repeat`, `transform`, `component_alpha`). Source-only
/// variants (`SolidFill`, gradients) carry no backing drawable;
/// `Drawable` resolves the host xid through `DrawableStore`.
///
/// Returns `None` if the picture xid isn't recorded or the
/// drawable backing has gone away. The engine treats this as a
/// gap and silently no-ops (matches v1's
/// `resolve_render_pic_with_gradient_xid` shape).
/// Stage 3f.14: depth-appropriate safe-default init colour for
/// fresh window storage when the X11 attribute `background-pixel`
/// is `None`. The v2 PixmapPool (3f.10) recycles
/// (image, view, memory) triples between drawables; a pool-take
/// inherits the returner's pixels, so leaving fresh storage at
/// pool content surfaces visually as widget-rect islands on
/// black (caja's drag artifact, 3f.10 + 3f.14 reproducer).
///
/// - Depth 32 windows are premultiplied-α; transparent black
///   `(0, 0, 0, 0)` is the no-op contribution to compositing.
/// - Depth 24 and other non-alpha visuals get opaque black
///   `(0, 0, 0, 1)` — matches "uninitialised window shows black"
///   which is the historical X11 behaviour clients expect.
///
/// The depth-24 arm is not cosmetic. We store depth-24 as
/// `B8G8R8A8_UNORM`, which has a real alpha byte; Xorg's pixman
/// representation has none, so a depth-24 image can never hold a
/// non-opaque alpha there at all — every fetch substitutes `0xff`
/// (`pixman-access.c:270-276`), and RENDER states the invariant
/// outright ("the destination alpha is always 1" for
/// `PICT_FORMAT_A(pDst->format) == 0`, `render/picture.c:1487-1488`).
/// If our storage starts at `α = 0`, a region the client never paints
/// reads back as a transparent hole that X11 says cannot exist, and a
/// depth-32 compositing client blends it. The composite write side of
/// the same rule is `render_pipeline::dst_color_write_mask`.
pub(in crate::kms::render::backend) fn default_window_init_color(depth: u8) -> [f32; 4] {
    if depth == 32 {
        [0.0, 0.0, 0.0, 0.0]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

impl KmsBackend {
    // ── Single-threaded core hooks ──────────────────────────────

    // Step 2 (DRIFT 2, findings 2026-06-18): the backend no longer
    // imposes server-side EWMH/focus stacking. Xorg restacks only via
    // ConfigureWindow/CirculateWindow (the WM drives EWMH stacking
    // through those), never from `_NET_WM_STATE`/_NET_WM_WINDOW_TYPE in
    // the server. The old `on_window_property_changed` /
    // `on_window_became_top_level` overrides called
    // `apply_top_level_stack_hint`, an independent z-order authority that
    // drifted from core (and `_NET_WM_STATE_FOCUSED → raise-to-top` was a
    // prime suspect for the wrong-raise bug). Both now fall back to the
    // trait's default no-op; top-level order is a pure projection of core
    // children via `sync_top_level_order`.
    pub(in crate::kms::render::backend) fn backend_windows_sync_top_level_order(
        &mut self,
        state: &ServerState,
    ) {
        use yserver_core::resources::ROOT_WINDOW;
        let mut order = Vec::new();
        for &child in state.resources.children(ROOT_WINDOW) {
            // Only host-backed children are drawable/orderable by the
            // backend; non-host-backed root children are reached (if ever)
            // by other means. Do NOT filter on map state — X11 stacking
            // order includes unmapped windows and must survive unmap/remap.
            let Some(host) = state
                .resources
                .window(child)
                .and_then(|w| w.host_xid)
                .map(|h| h.as_raw())
            else {
                continue;
            };
            if !self.windows.contains_key(&host) {
                // Benign transient: a core root child whose backend
                // registration/storage hasn't completed yet (or a failure
                // path). Project it anyway — order must survive — and LOG;
                // never panic (the scene + hit-test already skip xids
                // missing from windows). Codex review 2026-06-18.
                log::debug!(
                    target: "yserver::kms::render::stacking",
                    "sync_top_level_order: root child 0x{host:x} not (yet) in windows"
                );
            }
            order.push(host);
        }
        if self.core.top_level_order != order && !self.direct_frames_are_under_cow() {
            // A direct frame bypasses the composed root scene. A real
            // top-level restack changes that scene even when the direct
            // Present target itself is untouched, so retire it through the
            // normal composed replacement path before accepting another
            // direct Present.
            //
            // Not when every direct frame is the compositor's overlay window
            // or a descendant of it: the COW stacks above every top-level, so
            // a restack beneath it cannot change the screen. A compositing
            // desktop restacks constantly (raises, tooltips, notifications),
            // and each needless unflip showed a stale frame on Cinnamon.
            self.request_direct_unflip("top_level_stack_changed");
        }
        self.core.top_level_order = order;
        self.scene.wake_for_damage();
    }

    pub(in crate::kms::render::backend) fn backend_windows_create_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let xid = self.core.next_host_xid();
        let parent_xid = host_parent.as_raw();
        let parent_depth = if parent_xid == self.core.window_id {
            Some(24)
        } else {
            self.windows.get(&parent_xid).map(|g| g.depth)
        };
        let depth = depth_for_visual(visual, parent_depth);
        // Created unmapped, so no storage: `realize_window_storage` allocates it on viewability.
        self.register_window_geometry(
            xid,
            x,
            y,
            width.max(1),
            height.max(1),
            border_width,
            depth,
            Some(parent_xid),
            background_pixel,
        );
        if let Some(geom) = self.windows.get_mut(&xid)
            && let Some(bg_pix) = background_pixmap
        {
            geom.bg_pixmap = Some(bg_pix);
        }
        self.scene.wake_for_damage();
        WindowHandle::from_raw(xid).ok_or_else(|| io::Error::other("create_subwindow: xid was 0"))
    }

    pub(in crate::kms::render::backend) fn backend_windows_destroy_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        // A direct frame may still scan a source whose Copy fallback targets
        // this window. Request the composed replacement before dropping that
        // storage ownership; the frame's pins deliberately remain alive until
        // the replacement retires. An unrelated client-window destroy does
        // not invalidate the compositor's authoritative root-stage Present:
        // Cinnamon will replace it with its next Present, and forcing an
        // intermediate composed frame exposes the retained pre-direct BO.
        if self.direct_frame_references_host_drawable(host_xid) {
            self.request_direct_unflip("destroy_direct_frame_drawable");
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store_decref_with_invalidate(id);
        }
        if self
            .windows
            .remove(&host_xid)
            .is_some_and(|geom| geom.cursor.is_some())
        {
            // Xorg `DeleteWindow` drops the window's cursor ref (`dix/window.c:968`).
            self.refresh_effective_cursor();
            self.collect_released_cursors();
        }
        // Step 2 (DRIFT 2): top_level_order is no longer mutated here — it
        // is a projection of core children, reprojected by the destroy
        // core handler via `sync_top_level_order` after the resource child
        // is removed. (Scene already skips xids absent from windows, so
        // a transient stale entry between teardown and sync is harmless.)
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_map_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        if self.direct_frame_references_host_drawable(host_xid) {
            self.request_direct_unflip("map_direct_frame_drawable");
        }
        if let Some(geom) = self.windows.get_mut(&host_xid) {
            geom.mapped = true;
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store.set_scene_participating(id, true);
        }
        // The map-time background paint lives in `realize_window_storage`, driven by the delta.
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_realize_window_storage(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        if self.storage_lifecycle_exempt(host_xid) {
            return Ok(());
        }
        let Some(geom) = self.windows.get_mut(&host_xid) else {
            return Ok(());
        };
        geom.viewable = true;
        let geom = *geom;
        let leaf = self.allocate_window_leaf(host_xid);
        if let Some(id) = leaf {
            self.store.set_scene_participating(id, geom.mapped);
        }
        // Xorg miPaintWindow on RealizeTree: tile the background wherever the window paints.
        if geom.bg_pixel.is_some() || geom.bg_pixmap.is_some() {
            if let Err(e) = self.clear_window_area_with_background(
                host_xid,
                geom.bg_pixel.unwrap_or(0),
                geom.bg_pixmap,
                0,
                0,
                geom.width.max(1),
                geom.height.max(1),
                (0, 0),
            ) {
                log::debug!(
                    "render realize_window_storage: bg paint failed for 0x{host_xid:x}: {e:?}"
                );
            }
        } else if let Some(id) = leaf
            && self
                .resolve_paint_target(host_xid)
                .is_some_and(|t| t.backing_id() == id)
        {
            // Background None shows what is underneath: seed from the parent (no lower siblings).
            self.seed_backing_from_parent(host_xid, id);
        }
        // The ring, once core has sent the border source (it does right after CreateWindow).
        if geom.border_pixel.is_some() || geom.border_pixmap.is_some() {
            let tile_origin = self.border_tile_origin(host_xid);
            let _ = self.paint_window_border(host_xid, tile_origin);
        }
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_release_window_storage(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        if self.storage_lifecycle_exempt(host_xid) {
            return Ok(());
        }
        let Some(geom) = self.windows.get_mut(&host_xid) else {
            return Ok(());
        };
        geom.viewable = false;
        // A direct frame may still scan this leaf; its pins keep the image alive past the decref.
        if self.direct_frame_references_host_drawable(host_xid) {
            self.request_direct_unflip("release_direct_frame_drawable");
        }
        if let Some(id) = self.store.lookup(host_xid) {
            // Detach first: a pinned leaf survives the decref but must not stay the window's.
            self.store.detach_xid(host_xid);
            self.store_decref_with_invalidate(id);
        }
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_unmap_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        if self.direct_frame_references_host_drawable(host_xid) {
            self.request_direct_unflip("unmap_direct_frame_drawable");
        }
        if let Some(geom) = self.windows.get_mut(&host_xid) {
            geom.mapped = false;
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store.set_scene_participating(id, false);
        }
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_configure_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        // The compositor's root-stage Present remains authoritative while an
        // unrelated client window moves. The updated desktop arrives in its
        // next root Present, so replacing direct scanout for every
        // ConfigureWindow only creates direct/composed churn. Unwind when the
        // mutation touches storage retained by the active direct frame.
        if self.direct_frame_references_host_drawable(host_xid) {
            self.request_direct_unflip("configure_direct_frame_drawable");
        }
        let move_source = self.shared_backing_move_source(host_xid, &config);
        let Some(geom) = self.windows.get_mut(&host_xid) else {
            // Window not tracked — log + skip (e.g., configure
            // before register). v1 tolerates this.
            return Ok(());
        };
        let mut size_changed = false;
        if let Some(x) = config.x {
            geom.x = x;
        }
        if let Some(y) = config.y {
            geom.y = y;
        }
        if let Some(w) = config.width
            && w != geom.width
        {
            geom.width = w;
            size_changed = true;
        }
        if let Some(h) = config.height
            && h != geom.height
        {
            geom.height = h;
            size_changed = true;
        }
        // #133 step 2 (P3): mirror `border_width` from the configure.
        // Deliberately NOT folded into `size_changed`: the two need
        // different treatment of the pixels already in the storage, so
        // step 6 gives the border-width change its own path below.
        let mut border_width_changed = false;
        if let Some(bw) = config.border_width {
            border_width_changed = bw != geom.border_width;
            geom.border_width = bw;
        }
        // #133 step 6 (P8) — a `border_width` change is checked FIRST
        // and takes the whole configure with it, including a combined
        // `w`/`h` + `border_width` change. That combination is the
        // spec's worked case (`w=100,bw=2 → w=98,bw=3`, outer 104
        // either way): the resize path's compare-and-skip would find
        // the extent unchanged and leave the content at offset 2 while
        // every reader now expects 3.
        //
        // The two paths differ in what happens to the pixels, on
        // purpose (`LeafContent`): a border-width change always
        // preserves the client's drawable, because only the content's
        // position inside the storage moved; a resize preserves it only
        // for a window with no background, per the #143 block below.
        if border_width_changed {
            self.relayout_window_leaf_storage_for_border_change(host_xid);
        } else if size_changed
            && let Some(old_id) = self.store.lookup(host_xid)
            && self.store.redirected_target(old_id).is_none()
        {
            // #143 — a window with NO background keeps its pixels
            // across the reallocation. X11 says so for the default
            // gravity in the same breath as the discard: "The window is
            // tiled with its background. If no background is defined,
            // the existing screen contents are not altered"
            // (ForgetGravity, ChangeWindowAttributes), and Xorg
            // implements exactly that — the resize marks the whole
            // window exposed (`mi/miwindow.c:466-472`) and the paint
            // that follows returns without touching a pixel when the
            // window has none (`switch (pWin->backgroundState) { case
            // None: return; }`, `mi/miexpose.c:438-440`).
            //
            // This is the path an already-open window takes when the WM
            // retiles it, and discarding here is what made "already open
            // windows get broken rendering" when a compositor started
            // (#143): the window was wiped long before the redirect, and
            // the backing seed (`overlay_backing_inferiors`) then
            // faithfully copied the blank leaf. A shrink used to be
            // unrecoverable on top of that, because we emitted no Expose
            // for one; `handle_configure_window` now reports the whole
            // window exposed in either direction, as Xorg does
            // (`mi/miwindow.c:466-472`), so a discarded window WITH a
            // background gets asked to repaint.
            //
            // A window WITH a background is still discarded and re-tiled:
            // that IS the ForgetGravity rule, and it is what the
            // xeyes-resize regression (2026-05-16,
            // `subwindow_resize_clears_old_paint`) needs. Honouring a
            // non-Forget `bit_gravity` would keep those pixels too, but
            // the attribute does not reach this backend today.
            //
            // Reallocates and repaints the ring itself — see
            // `sync_window_leaf_storage`.
            let content = if self
                .windows
                .get(&host_xid)
                .is_some_and(|g| g.bg_pixel.is_none() && g.bg_pixmap.is_none())
            {
                LeafContent::Migrate
            } else {
                LeafContent::Discard
            };
            self.sync_window_leaf_storage(host_xid, content);
        }
        if let Some(stack_mode) = config.stack_mode {
            // Top-level z-order is no longer mutated here: it is a pure
            // projection of core children, reprojected by the core
            // ConfigureWindow handler via `sync_top_level_order` (Step 2,
            // DRIFT 2). Only subwindow sibling order (`stack_rank`) stays
            // backend-maintained here (Step 2b). A subwindow is one with a
            // tracked parent.
            let is_subwindow = self
                .windows
                .get(&host_xid)
                .is_some_and(|g| g.parent.is_some());
            if is_subwindow {
                self.restack_subwindow(host_xid, stack_mode, config.sibling);
            }
        }
        // After the restack, so the destination clip sees the new
        // stacking, as Xorg's `CopyWindow` runs against the validated
        // tree.
        if let Some(source) = move_source {
            self.carry_shared_backing_pixels_on_move(host_xid, source);
        }
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_reparent_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        // Stage 3f.6: update the parent xid so build_scene's
        // descendant traversal sees the new tree shape on the next
        // tick. BOTH `host_parent == 0` and `host_parent ==
        // core.window_id` (root's real host xid; root is never tracked
        // in `windows`) mean the window becomes a top-level under
        // root; we record `None` so the recurse treats it as a top-
        // level entry. A genuinely-unknown non-root xid is projection
        // drift between resources and backend and panics below.
        //
        // Stage 3f.11 bug-fix: also reconcile `core.top_level_order`
        // with the new parent. Pre-3f.11, an xid that was originally
        // registered as a top-level (parent=root) stayed in
        // `top_level_order` even after being reparented under
        // another window. `build_scene` then emitted the same xid
        // TWICE: once via the `top_level_order` walk (at its now-
        // child-relative coords interpreted as absolute → typically
        // (0,0)) and once via the recurse from its real parent (at
        // its correct screen position). Observable as MATE's clock
        // applet rendered at BOTH ends of the panel: the right edge
        // is the real position, the left edge is the ghost.
        let parent = if host_parent == 0 || host_parent == self.core.window_id {
            // BOTH sentinels mean "top-level under root": `0` is the
            // legacy convention; `core.window_id` is root's real host
            // xid (root is never tracked in windows). The reparent-
            // to-root path passes backend.window_id() (== core.window_id),
            // NOT 0 — so this second clause is load-bearing. (Same root-
            // sentinel check as backend.rs:1668.)
            None
        } else if self.windows.contains_key(&host_parent) {
            Some(host_parent)
        } else {
            // Per spec §"Remove the missing-parent fallback": backend
            // projection drift after protocol-level validation is a
            // fatal internal-consistency failure, not a silent
            // recovery. If the resources tree says the parent exists
            // but windows doesn't, that's drift — surface it.
            panic!(
                "reparent_subwindow: host_parent 0x{host_parent:x} missing from \
                 windows (and is neither 0 nor root/core.window_id); resources \
                 layer must validate ReparentWindow before dispatching to backend"
            );
        };
        let new_rank = self.alloc_window_stack_rank();
        if let Some(geom) = self.windows.get_mut(&host_xid) {
            geom.x = x;
            geom.y = y;
            // The parent update is load-bearing — `build_scene` recurses by
            // `windows.parent`, so this is what prevents a reparented
            // window from being double-emitted (once via the top-level walk
            // and once via the recurse).
            geom.parent = parent;
            geom.stack_rank = new_rank;
        }
        // Step 2 (DRIFT 2): top_level_order is no longer reconciled here —
        // it projects core children, reprojected by the reparent core
        // handler via `sync_top_level_order` after the core tree moves the
        // window (across the root boundary).
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_change_subwindow_attributes(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        // Stage 3f.6: v1-shape parse of the CWA value-mask.
        // CWBackPixmap (0x01), CWBackPixel (0x02), CWBorderPixmap
        // (0x04) and CWBorderPixel (0x08) are the four we honour —
        // they decide what fresh / cleared regions of the window
        // storage and (from step 4) the border ring look like. Other
        // CW bits (CWBitGravity, CWEventMask, CWCursor, ...) flow
        // through other Backend methods or get folded into broader
        // window state; storing only what `windows` needs.
        //
        // The value list is positional: X11 orders values by ascending
        // mask bit (`dix/window.c:1182` walks the mask with
        // `lowbit(tmask)`), so `idx` must advance in the same order.
        // Adding a bit out of order would silently mis-read every
        // later value in a multi-attribute CWA.
        let Some(geom) = self.windows.get_mut(&host_xid) else {
            return Ok(());
        };
        let mut idx = 0;
        if value_mask & 0x01 != 0 && idx < values.len() {
            // CWBackPixmap. 0 = None / inherit-from-parent.
            let v = values[idx];
            geom.bg_pixmap = if v == 0 { None } else { Some(v) };
            idx += 1;
        }
        if value_mask & 0x02 != 0 && idx < values.len() {
            // CWBackPixel — opaque ARGB-or-XRGB pixel value.
            geom.bg_pixel = Some(values[idx]);
            idx += 1;
        }
        // #133 step 2 (P3): the border source. Xorg's border is an
        // either/or (`PixUnion border` + `borderIsPixel`,
        // `include/windowstr.h:146`), and core resolves CopyFromParent
        // and pixel-overrides-pixmap before forwarding, so exactly one
        // of these bits arrives per change and the other slot is
        // cleared to keep the mirror an either/or too.
        if value_mask & 0x04 != 0 && idx < values.len() {
            // CWBorderPixmap — raw host pixmap xid of the border tile.
            let v = values[idx];
            geom.border_pixmap = if v == 0 { None } else { Some(v) };
            if geom.border_pixmap.is_some() {
                geom.border_pixel = None;
            }
            idx += 1;
        }
        if value_mask & 0x08 != 0 && idx < values.len() {
            // CWBorderPixel — solid border colour.
            geom.border_pixel = Some(values[idx]);
            geom.border_pixmap = None;
        }
        // X11 spec: CWA's background attribute change does NOT
        // repaint the window. The bg setting only affects future
        // `ClearArea` / Expose handling. v2's pre-2026-05-30 eager
        // clear here was a Stage 3f.6 over-reach: the Stage 4d
        // guard (`routes_via_redirect`) skipped the clear for
        // windows under COMPOSITE redirect (avoiding the
        // "CC opaque black on drag with compositing" and
        // "tray applets disappear" symptoms), but the
        // non-redirected path still cleared — visible as
        // non-composited MATE's CC sidebar going black when caja
        // took focus over it (marco re-asserts CWA per configure;
        // yserver wiped CC's pixmap to bg=0; GTK got no Expose so
        // bg never repainted; widgets came back only on
        // per-widget hover redraw). Removing the clear matches
        // X11 in both modes.
        //
        // #133 step 4 (4.4) — the BORDER is the opposite case: X11 says
        // a border-attribute change DOES repaint the border. Xorg does
        // it right here, in `ChangeWindowAttributes` itself, after the
        // ddx hook and gated on `(CWBorderPixel | CWBorderPixmap)`
        // (`dix/window.c:1584-1591`, comment: "If the border contents
        // have changed, redraw the border"). This is also the CREATION
        // trigger: core forwards the resolved border source through
        // this same route immediately after `create_subwindow`
        // (`process_request.rs:20541`), including the CreateWindow
        // inherit-from-parent case (`dix/window.c:879`).
        if value_mask & 0x0c != 0 {
            let tile_origin = self.border_tile_origin(host_xid);
            let _ = self.paint_window_border(host_xid, tile_origin);
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_register_top_level(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        // Bookkeeping mutation — same shape as v1. The XID map is in
        // KmsCore and shared.
        self.core.xid_map.insert(host_xid, nested_id);
        // Top-level visible-window tracking for the scene
        // assembler. register_top_level doesn't carry geometry;
        // start at 1x1 (Stage 2 plan compromise) and resize on
        // first configure_subwindow.
        if !self.windows.contains_key(&host_xid) {
            // Top-level: parent = None (root), no bg_pixel known yet
            // (set later via change_subwindow_attributes).
            self.register_window_geometry(host_xid, 0, 0, 1, 1, 0, 24, None, None);
        }
        // Step 2 (DRIFT 2): top_level_order membership is no longer set
        // here — the create / reparent-to-root core handlers reproject it
        // from core children via `sync_top_level_order` after this call.
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_register_subwindow(
        &mut self,
        _origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.core.xid_map.insert(host_xid, nested_id);
        if !self.windows.contains_key(&host_xid) {
            // register_subwindow doesn't carry parent xid (Backend
            // trait doesn't expose it here — the trait shape was
            // built around v1's flat windows table). Parent is set
            // when `create_subwindow` fires for the same host_xid
            // (it's the entry point that knows the parent). If
            // register_subwindow runs first (e.g. ynest's wire
            // ordering), we'll get `None` and the scene treats this
            // window as a top-level until a `create_subwindow`
            // catches up. Matches v1's "no parent tracking" status
            // — v1 simply doesn't compose children either.
            self.register_window_geometry(host_xid, 0, 0, 1, 1, 0, 32, None, None);
        }
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_paint_window_background_rect(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let Some(geom) = self.windows.get(&host_xid) else {
            return Ok(());
        };
        let (bg_pixel, bg_pixmap) = (geom.bg_pixel, geom.bg_pixmap);
        if bg_pixel.is_none() && bg_pixmap.is_none() {
            // Background None: contents stay undefined (miPaintWindow
            // early-out).
            return Ok(());
        }
        self.clear_window_area_with_background(
            host_xid,
            bg_pixel.unwrap_or(0),
            bg_pixmap,
            x,
            y,
            width,
            height,
            (0, 0),
        )
    }

    // ── Container background ────────────────────────────────────
    pub(in crate::kms::render::backend) fn backend_windows_set_container_background_pixel(
        &mut self,
        _origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.core.bg_pixel = Some(pixel);
        self.core.bg_pixmap = None;
        // Stage 4a — root paint resolves through redirect routing.
        // In the common (unredirected) case this is the leaf root
        // drawable; if a compositor has redirected root, paint
        // lands in its backing instead. `resolve_paint_target`
        // returns `None` only when the root xid isn't in the
        // store, which is a fixture-init bug.
        if let Some(target) = self.resolve_paint_target(self.core.window_id) {
            let rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: target.offset().0,
                    y: target.offset().1,
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(self.platform.fb_w.max(1)),
                    height: u32::from(self.platform.fb_h.max(1)),
                },
            };
            // L1 server-α invariant: root storage is depth-24, so
            // force the stored α byte to 0xFF for the scene
            // compositor's pass-through draw to read opaque.
            let depth = self
                .store
                .get(target.backing_id())
                .map(|d| d.depth)
                .unwrap_or(24);
            let format = self
                .store
                .get(target.backing_id())
                .map(|d| d.storage.format)
                .unwrap_or_else(|| PlatformBackend::format_for_depth(depth));
            if let Err(e) = self.engine.fill_rect(
                &mut self.store,
                &mut self.platform,
                target.dst(),
                rect,
                decode_x11_pixel_for_storage(pixel, depth, format),
            ) {
                log::warn!("render set_container_background_pixel: root fill failed: {e:?}");
            } else {
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::FillOne, target.backing_id(), 1);
            }
        }
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_set_container_background_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        self.core.bg_pixmap = PixmapHandle::from_raw(host_pixmap_xid);
        self.core.bg_pixel = None;
        self.tile_root_background_pixmap(host_pixmap_xid);
        self.scene.wake_for_damage();
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_set_shape_rectangles(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: Option<&[xfixes::RegionRect]>,
    ) -> io::Result<()> {
        // DIAG(#98): a compositor that unredirects a fullscreen window is
        // expected to punch a matching hole in the COW (mutter-lineage
        // `shape_cow_for_window` → XFixesSetWindowShapeRegion with the
        // inverse of the window rect). Log every shape mutation so a
        // failing fullscreen run distinguishes the two candidate
        // mechanisms: muffin shapes the COW, versus muffin shapes nothing
        // and the server's own COW-suppression probe is the only thing that
        // can reveal the window.
        //
        // The parenthetical this comment used to carry — "the scene clips
        // children by the parent RECT, never by the parent's bounding SHAPE"
        // — is no longer true and was corrected while auditing #133 step 5.
        // Under `Visibility::On` the walk clips a node's children to the
        // parent's `mine` region, which is built from its shape-clipped
        // place rects (`scene.rs`, `visit_window_subtree` step 2), so an
        // empty bounding shape prunes the whole subtree and a partial one
        // clips it — see `build_scene_empty_bounding_emits_no_draw` and
        // `a_partial_parent_shape_clips_its_children`. #133 step 5 narrowed
        // that region further, from the parent's `borderSize` to its
        // `winSize`.
        log::debug!(
            "cow_diag: set_shape_rectangles host_xid=0x{host_xid:x} kind={kind} \
             n_rects={n:?} rects={first:?}",
            n = rects.map(<[xfixes::RegionRect]>::len),
            first = rects.map(|r| &r[..r.len().min(4)]),
        );
        // Bookkeeping mutation: SHAPE rects live in KmsCore as a faithful
        // projection of the core resource tree. The `Option` preserves
        // the empty-vs-absent distinction (DRIFT 1): `None` removes the
        // entry (unset → full window / live geometry); `Some(rects)`
        // stores the region verbatim, INCLUDING `Some([])` (explicit
        // empty → click-through for input, drawn-as-nothing for bounding).
        // `cursor_inside_shape` and the scene's bounding clip already read
        // `Some([])` correctly; the old API deleted on empty and lost it.
        self.shape_generation = self.shape_generation.wrapping_add(1);
        let dst = match kind {
            0 => &mut self.core.shape_bounding,
            1 => &mut self.core.shape_clip,
            2 => &mut self.core.shape_input,
            _ => {
                self.log_render_gap("set_shape_rectangles_invalid_kind");
                return Ok(());
            }
        };
        match rects {
            None => {
                dst.remove(&host_xid);
            }
            Some(rects) => {
                dst.insert(host_xid, rects.to_vec());
            }
        }
        // Bounding (0) and clip (1) shapes change what the scene draws,
        // so wake the compositor. Without this a shape change didn't
        // repaint until an unrelated event (latent bug); and it un-
        // strands a window flagged `offscreen_no_draw` for an empty
        // bounding shape once the shape becomes non-empty (idle free-run
        // fix cut 2b — the compose scheduler otherwise excludes it).
        // Input shape (2) only affects hit-testing — no redraw needed.
        if kind == 0 || kind == 1 {
            if self.direct_frames_shaped_off_root() {
                self.request_direct_unflip("shape_clips_direct_frame");
            }
            self.scene.wake_for_damage();
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_windows_windows_restructured(
        &mut self,
        state: &mut ServerState,
    ) {
        // Xorg `CheckMotion(NULL)`: the sprite starts on the root, and only a
        // changed pointer window generates crossings — one hit-test otherwise.
        let host_xid = self.resource_pointer_host_xid(state);
        let prev = *self
            .core
            .prev_pointer_window
            .get_or_insert(self.core.window_id);
        if prev == host_xid {
            // The window under the pointer is the same, but an InputOnly
            // window's cursor (kept here, not by `define_cursor`) may
            // have changed.
            self.refresh_effective_cursor();
            return;
        }
        let mask = self.serialize_modifiers() | self.core.button_mask;
        self.update_pointer_window(
            state,
            host_xid,
            mask,
            yserver_core::core_loop::InputOrigin::NestedHost,
        );
        let pending = std::mem::take(&mut self.core.pending_pointer_events);
        let xid_map = self.core.xid_map.clone();
        for mut ev in pending {
            ev.tree_change = true;
            let _dropped = yserver_core::core_loop::pointer_fanout::pointer_event_fanout_to_state(
                state, self, &xid_map, ev, true, false,
            );
        }
    }
}
