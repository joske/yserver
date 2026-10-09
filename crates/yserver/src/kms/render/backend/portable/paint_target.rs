use super::*;

impl KmsBackend {
    pub(in crate::kms::render::backend) fn init_root_storage(&mut self) {
        let root_xid = self.core.window_id;
        if self.store.lookup(root_xid).is_some() {
            return;
        }
        let width = self.platform.fb_w.max(1);
        let height = self.platform.fb_h.max(1);
        let storage = match self.platform.allocate_drawable_storage_as(
            width,
            height,
            32,
            crate::kms::vk::mem_accounting::MemCategory::WindowStorage,
        ) {
            Ok(storage) => {
                self.telemetry.record_storage_allocation();
                self.telemetry.record_image_view_create();
                storage
            }
            Err(e) => {
                log::debug!("render init_root_storage: no Vk, using stub root storage: {e:?}");
                Storage::for_tests_null(
                    ash::vk::Extent2D {
                        width: u32::from(width),
                        height: u32::from(height),
                    },
                    PlatformBackend::format_for_depth(32),
                )
            }
        };
        let id = match self.store_alloc(root_xid, DrawableKind::Root, 32, true, storage) {
            Ok(id) => id,
            Err(e) => {
                log::warn!("render init_root_storage: store.allocate failed: {e:?}");
                return;
            }
        };
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        if let Err(e) = self.engine.fill_rect(
            &mut self.store,
            &mut self.platform,
            Dst::server_internal(id),
            rect,
            decode_x11_pixel_for_storage(
                self.core
                    .bg_pixel
                    .unwrap_or(yserver_core::resources::ROOT_DEFAULT_BACKGROUND_PIXEL),
                24,
                PlatformBackend::format_for_depth(24),
            ),
        ) && self.platform.vk.is_some()
        {
            log::warn!("render init_root_storage: initial root fill failed: {e:?}");
        }
    }

    /// Stamp alpha = 0xFF over `rects` (storage coordinates) when `target`
    /// is a depth-24 drawable resolved into a depth-32 ancestor's redirect
    /// backing; a no-op for every other target.
    ///
    /// A depth-24 window has no alpha, so its pixels must read opaque in
    /// that backing — Xorg composites a mismatched-depth child into its
    /// parent with alpha forced to 1. The fills and RENDER paths already
    /// honour that; a raw image copy does not, and carries the source's
    /// undefined X byte across (a GL client's background is commonly 0,
    /// which picom then shows through). Called after each raw copy with
    /// the rects it wrote.
    pub(in crate::kms::render::backend) fn stamp_opaque_alpha_if_shared(
        &mut self,
        target: PaintTarget,
        rects: &[ash::vk::Rect2D],
    ) {
        if rects.is_empty() || target.x11_depth() != 24 {
            return;
        }
        if !self
            .store
            .get(target.backing_id())
            .is_some_and(|d| d.depth == 32)
        {
            return;
        }
        let rects16: Vec<Rectangle16> = rects
            .iter()
            .filter_map(|r| {
                Some(Rectangle16 {
                    x: i16::try_from(r.offset.x).ok()?,
                    y: i16::try_from(r.offset.y).ok()?,
                    width: u16::try_from(r.extent.width).ok()?,
                    height: u16::try_from(r.extent.height).ok()?,
                })
            })
            .collect();
        if let Err(e) = self.engine.stamp_opaque_alpha(
            &mut self.store,
            &mut self.platform,
            target.dst(),
            &rects16,
        ) {
            log::warn!(
                "render: stamping opaque alpha into depth-32 backing {:?} failed: {e:?}",
                target.backing_id()
            );
        }
    }

    /// Stage 4a — resolve a host xid into the actual paint target
    /// under COMPOSITE redirect routing. Walks up the
    /// `windows.parent` chain accumulating `(x, y)` offsets;
    /// the first ancestor (including `host_xid` itself) whose
    /// `Drawable.redirected_target` is `Some(B_id)` wins.
    ///
    /// Returns:
    /// - `None` if `host_xid` doesn't map to any drawable.
    /// - the LEAF drawable for Pixmap targets (not in `windows`) and for
    ///   unredirected windows whose ancestor chain reaches root without
    ///   finding a redirected ancestor.
    /// - the redirect BACKING for redirected windows + their descendants.
    ///
    /// #133 step 3 (P4): the walk also accumulates the CONTENT offset
    /// (`border_width` per level, since a window's storage starts at its
    /// OUTER origin — `compAllocPixmap`, `composite/compalloc.c:610`)
    /// and the content clip. Both collapse to the pre-#133 values —
    /// offset `(0, 0)` / clip `None` — when every window in the chain
    /// has `border_width == 0`. See [`PaintTarget`] for the four cases.
    ///
    /// Per Stage 4 plan §"Per-hierarchy redirect": this is the
    /// per-op walk; tree depth bounds cost (typically ≤ 4 for
    /// real apps). Cached-ancestry alternative deferred to
    /// Stage 5 if profiling shows it.
    pub(crate) fn resolve_paint_target(&self, host_xid: u32) -> Option<PaintTarget> {
        let leaf_id = self.store.lookup(host_xid);
        let leaf_depth = self
            .windows
            .get(&host_xid)
            .map(|w| w.depth)
            .or_else(|| leaf_id.and_then(|id| self.store.get(id).map(|d| d.depth)))?;
        if let Some(geom) = self.windows.get(&host_xid) {
            // An unviewable window is clipped away, even under a viewable redirected ancestor.
            if !geom.viewable && !self.storage_lifecycle_exempt(host_xid) {
                return None;
            }
            self.resolve_window_paint_target(host_xid, leaf_id, leaf_depth)
        } else {
            let leaf_id = leaf_id?;
            self.resolve_non_window_paint_target(leaf_id, leaf_depth)
        }
    }

    fn resolve_non_window_paint_target(
        &self,
        leaf_id: crate::kms::render::store::DrawableId,
        leaf_depth: u8,
    ) -> Option<PaintTarget> {
        // Pixmaps have no border (`content == None`: the whole storage
        // is content), so this arm is unchanged by #133.
        if let Some(b_id) = self.store.redirected_target(leaf_id) {
            return Some(PaintTarget::new(b_id, (0, 0), None, leaf_depth));
        }
        Some(PaintTarget::new(leaf_id, (0, 0), None, leaf_depth))
    }

    pub(in crate::kms::render::backend) fn resolve_window_paint_target(
        &self,
        host_xid: u32,
        leaf_id: Option<crate::kms::render::store::DrawableId>,
        leaf_depth: u8,
    ) -> Option<PaintTarget> {
        use crate::kms::render::target::ContentClipAccum;

        let mut cur_xid = host_xid;
        // #133 step 3 (P4/3.3) — the walk carries TWO accumulators, both
        // expressed in the frame it has reached (initially: the drawn
        // window's own outer origin, which is where its storage starts,
        // per `compAllocPixmap` `composite/compalloc.c:610`).
        //
        //   offset : the drawn window's CONTENT origin
        //   clip   : the content rects of every BORDERED level, intersected
        //
        // Per level the recurrence is `+ cur.x` (into the parent's content
        // frame) then `+ parent.border_width` (into the parent's outer
        // frame), which is the spec's one-level translation
        // `W.border_width + C.x + C.border_width` for a child C painting
        // into redirected ancestor W.
        //
        // A level with `border_width == 0` contributes NO clip term. Its
        // content rect is not a border constraint, and adding it would
        // narrow the `bw == 0` path — where the storage extent is the only
        // clip today — on every desktop we support.
        let own_bw = self.window_border_width(host_xid);
        let mut offset = (own_bw, own_bw);
        // #133 step 3 round 6 — every term that comes from a window's
        // own STORAGE (the leaf case, and a redirect owner's) is taken
        // from what that storage was ALLOCATED with, never from the
        // current `border_width`: see `storage_content_offset` for why
        // (an allocation that has not followed a border-width change
        // would otherwise displace every pixel already drawn). The
        // intermediate terms below are pure geometry — they position a
        // descendant inside an ancestor's content and own no storage.
        let mut clip = ContentClipAccum::default();
        if let Some(g) = self.windows.get(&host_xid)
            && g.border_width > 0
        {
            clip.intersect(crate::kms::render::target::content_rect(
                offset, g.width, g.height,
            ));
        }
        // The window's own content intersected with every ancestor's,
        // bordered or not: its clip in an ANCESTOR's backing, which it
        // shares with its parent and siblings (see
        // `PaintTarget::within_window_bounds`). Same frames as `clip`.
        let mut bounds = ContentClipAccum::default();
        if let Some(g) = self.windows.get(&host_xid) {
            bounds.intersect(crate::kms::render::target::content_rect(
                offset, g.width, g.height,
            ));
        }
        loop {
            if let Some(cur_id) = self.store.lookup(cur_xid)
                && let Some(b_id) = self.store.redirected_target(cur_id)
            {
                // A redirected window owns backing sized to its OWN
                // bordered geometry and is clipped to its own content
                // only — never to its parent. Xorg keys this off
                // `redirectDraw != RedirectDrawNone`, with no
                // manual/automatic distinction (`SetWinSize`,
                // `dix/window.c:1720`; `SetBorderSize`, `:1747`).
                // #133 step 3 round 6: the OWNER's own border term must
                // come from what its BACKING was allocated with, not
                // from its current `border_width` — same rule as the
                // leaf case (see `storage_content_offset`). Re-base the
                // accumulation by the delta; everything inside the
                // owner's content (all descendant terms, and the clip)
                // moves with it.
                let geom_bw = self.window_border_width(cur_xid);
                let layout_bw = self.storage_content_offset(cur_xid, b_id);
                let delta = layout_bw - geom_bw;
                let mut clip = clip;
                clip.shift(delta, delta);
                let mut bounds = bounds;
                bounds.shift(delta, delta);
                if layout_bw > 0 && geom_bw == 0 {
                    // The owner's own content term was never folded in
                    // (its geometry says `bw == 0`), but its backing is
                    // laid out with a border: add it now, in backing
                    // coordinates.
                    if let Some(g) = self.windows.get(&cur_xid) {
                        clip.intersect(crate::kms::render::target::content_rect(
                            (layout_bw, layout_bw),
                            g.width,
                            g.height,
                        ));
                    }
                }
                let target = PaintTarget::new(
                    b_id,
                    (offset.0 + delta, offset.1 + delta),
                    self.finish_content_clip(clip, b_id),
                    leaf_depth,
                );
                if cur_xid == host_xid {
                    return Some(target);
                }
                return Some(target.within_window_bounds(self.finish_content_clip(bounds, b_id)));
            }
            // No `windows` entry means we've stepped onto root
            // (parent = `core.window_id`, not tracked) or onto an
            // unparented orphan. In both cases there's no parent
            // chain left to walk; return identity at the leaf.
            // if it still has live storage. If the leaf drawable
            // has already detached from the xid map (e.g. a
            // redirected descendant whose paints should route only
            // via an ancestor backing), keep the redirected-ancestor
            // miss as `None`.
            let Some(geom) = self.windows.get(&cur_xid) else {
                return leaf_id.map(|id| self.leaf_paint_target(host_xid, id, leaf_depth));
            };
            match geom.parent {
                None => {
                    // Top-level: parent is root, not tracked in
                    // `windows`. `create_subwindow` records
                    // `parent = None` when the host_parent is
                    // root_xid (the if-not-in-windows branch),
                    // so this is the production representation
                    // for every top-level. Step up to root
                    // explicitly so a `RedirectWindow(root, …)`
                    // compositor sees top-level descendants route
                    // through the root backing — codex round-7
                    // finding (`parent == None` previously
                    // returned identity without consulting root).
                    offset.0 += i32::from(geom.x);
                    offset.1 += i32::from(geom.y);
                    clip.shift(i32::from(geom.x), i32::from(geom.y));
                    bounds.shift(i32::from(geom.x), i32::from(geom.y));
                    // The root window has no border (spec §Invariants), so
                    // its content origin IS its backing origin: no further
                    // shift and no clip term.
                    if let Some(root_id) = self.store.lookup(self.core.window_id)
                        && let Some(b_id) = self.store.redirected_target(root_id)
                    {
                        // Root has no border, so nothing to re-base
                        // here: the accumulated offset already ends in
                        // the root's content frame, which IS its
                        // backing origin.
                        return Some(
                            PaintTarget::new(
                                b_id,
                                offset,
                                self.finish_content_clip(clip, b_id),
                                leaf_depth,
                            )
                            .within_window_bounds(self.finish_content_clip(bounds, b_id)),
                        );
                    }
                    // No root redirect: paint stays on the leaf
                    // at its own CONTENT origin if the leaf storage is
                    // still live. Explicit match (not `?`) so we
                    // don't poison the outer Option.
                    return leaf_id.map(|id| self.leaf_paint_target(host_xid, id, leaf_depth));
                }
                Some(parent_xid) => {
                    // Into the parent's CONTENT frame…
                    offset.0 += i32::from(geom.x);
                    offset.1 += i32::from(geom.y);
                    clip.shift(i32::from(geom.x), i32::from(geom.y));
                    bounds.shift(i32::from(geom.x), i32::from(geom.y));
                    // …intersect the parent's own content rect (a child
                    // may not reach its parent's border,
                    // `mi/mivaltree.c:386`), then step into the parent's
                    // OUTER frame, which is where the parent's storage or
                    // backing starts.
                    let parent_bw = self.window_border_width(parent_xid);
                    if parent_bw > 0
                        && let Some(pg) = self.windows.get(&parent_xid)
                    {
                        clip.intersect(crate::kms::render::target::content_rect(
                            (0, 0),
                            pg.width,
                            pg.height,
                        ));
                    }
                    if let Some(pg) = self.windows.get(&parent_xid) {
                        bounds.intersect(crate::kms::render::target::content_rect(
                            (0, 0),
                            pg.width,
                            pg.height,
                        ));
                    }
                    offset.0 += parent_bw;
                    offset.1 += parent_bw;
                    clip.shift(parent_bw, parent_bw);
                    bounds.shift(parent_bw, parent_bw);
                    cur_xid = parent_xid;
                }
            }
        }
    }

    /// `border_width` of a tracked window as an `i32`, or 0 for the root
    /// / an untracked xid (#133: the root window has no border).
    pub(in crate::kms::render::backend) fn window_border_width(&self, host_xid: u32) -> i32 {
        self.windows
            .get(&host_xid)
            .map_or(0, |g| i32::from(g.border_width))
    }

    /// Resolve an accumulated content clip against the storage it will be
    /// applied in. `None` (no bordered level) keeps the target on the
    /// pre-#133 arithmetic.
    fn finish_content_clip(
        &self,
        clip: crate::kms::render::target::ContentClipAccum,
        id: DrawableId,
    ) -> Option<ash::vk::Rect2D> {
        let extent = self.store.get(id).map(|d| d.storage.extent)?;
        clip.finish(extent)
    }

    /// #133 step 3 round 6 — the content offset the STORAGE was
    /// ALLOCATED with (`compAllocPixmap`,
    /// `composite/compalloc.c:610`): the client-visible content starts
    /// this many pixels inside it.
    ///
    /// **The content layout is a property of the allocation, not of the
    /// current `border_width`.** A `border_width` change mirrors into
    /// the geometry immediately, but reallocating and MIGRATING the
    /// content is step 6 (P8) — `configure_subwindow` re-syncs storage
    /// only on a width/height change. Re-basing the content origin off
    /// the new `border_width` alone moves the coordinate system without
    /// moving a single pixel, so everything already drawn is displaced
    /// by the delta.
    ///
    /// That was the xts5 Xlib9 `IncludeInferiors` regression, all 14 of
    /// them: `makewin` creates the test window with `border_width = 1`
    /// (xts5/src/lib/makewin2.c:232) and the purpose draws into it, then
    /// its root section does `XSetWindowBorderWidth(A_DRAW, 0)` before
    /// re-drawing through the root and comparing against the earlier
    /// saved image. Everything drawn before the change sat at storage
    /// `+(1, 1)`; after it the same content coordinates addressed
    /// storage `+(0, 0)`. The intervening `dclear` is clipped by the
    /// strip children — `dset` builds a FRESH GC, so ClipByChildren
    /// (xts5/src/lib/dset.c:126-149) — so the 1-px-displaced residue
    /// under the last strip survived and surfaced one column right of
    /// the rect: `Pixel mismatch at (90, 31)`.
    ///
    /// Anchoring on the allocation keeps every pixel where it is until
    /// something reallocates, and leaves step 6's realloc-and-migrate
    /// ([`Self::relayout_window_leaf_storage_for_border_change`]) as
    /// the single place that ever re-bases content — and it moves the
    /// pixels in the same breath. Identity at `bw == 0`, and equal to
    /// `border_width` whenever the two agree, which is always for a
    /// window that owns its own leaf storage. A window whose allocation
    /// could not follow (a core-owned redirect backing, a failed
    /// allocation) is where the two can still differ, and this is the
    /// value that keeps such a window readable rather than displaced.
    ///
    /// The value is RECORDED at allocation
    /// ([`Drawable::content_offset`]), not inferred from the extent: a
    /// redirect backing may legitimately be larger than its window, so
    /// an extent difference does not imply a border.
    pub(in crate::kms::render::backend) fn storage_content_offset(
        &self,
        _host_xid: u32,
        id: DrawableId,
    ) -> i32 {
        self.store.get(id).map_or(0, |d| d.content_offset)
    }

    /// The paint target for a window drawing into its OWN leaf storage.
    /// The content origin comes from the ALLOCATION
    /// ([`Self::storage_content_offset`]), not from `border_width`.
    fn leaf_paint_target(&self, host_xid: u32, id: DrawableId, leaf_depth: u8) -> PaintTarget {
        let b = self.storage_content_offset(host_xid, id);
        PaintTarget::new(
            id,
            (b, b),
            self.leaf_content_clip(host_xid, id, b),
            leaf_depth,
        )
    }

    /// The content clip for a window painting into its OWN leaf storage:
    /// `(b, b, w, h)` inside `(w + 2b) x (h + 2b)` storage, where `b` is
    /// the ALLOCATION's content offset. `None` at `b == 0`, where
    /// content and storage coincide.
    fn leaf_content_clip(&self, host_xid: u32, id: DrawableId, b: i32) -> Option<ash::vk::Rect2D> {
        if b == 0 {
            return None;
        }
        let geom = self.windows.get(&host_xid)?;
        let mut clip = crate::kms::render::target::ContentClipAccum::default();
        clip.intersect(crate::kms::render::target::content_rect(
            (b, b),
            geom.width,
            geom.height,
        ));
        self.finish_content_clip(clip, id)
    }

    /// What a pure move of `host_xid` has to carry along, captured
    /// BEFORE the configure mutates its geometry: `None` unless the
    /// window draws into an ANCESTOR's redirect backing.
    ///
    /// A window under a redirected ancestor has no pixels of its own on
    /// screen: it (and every non-redirected inferior) paints straight
    /// into the shared backing at its offset, so moving it leaves those
    /// pixels behind. Xorg moves them inside that one pixmap:
    /// `miMoveWindow` calls `CopyWindow` (`mi/miwindow.c:293`), and
    /// `compCopyWindow` falls through for a window that is not itself
    /// redirected (`composite/compwindow.c:548-552`) to `fbCopyWindow`,
    /// which copies the old `borderClip` to the new origin. The client
    /// is not asked to repaint the part that copy covers, so a client
    /// that drew once — the MATE notification area composites each
    /// tray icon into its window and then waits for Damage — shows
    /// whatever the backing held at the new position. A window that
    /// paints into its own leaf (no redirected ancestor), or owns its
    /// own backing, moves with its storage and needs nothing.
    ///
    /// Only a pure move qualifies: a resize or border-width change
    /// reallocates or re-tiles the window and goes through the paths
    /// below it instead.
    pub(in crate::kms::render::backend) fn shared_backing_move_source(
        &self,
        host_xid: u32,
        config: &HostSubwindowConfig,
    ) -> Option<SharedBackingMoveSource> {
        let geom = self.windows.get(&host_xid)?;
        let moved = config.x.is_some_and(|x| x != geom.x) || config.y.is_some_and(|y| y != geom.y);
        let resized = config.width.is_some_and(|w| w != geom.width)
            || config.height.is_some_and(|h| h != geom.height)
            || config
                .border_width
                .is_some_and(|bw| bw != geom.border_width);
        if !moved || resized {
            return None;
        }
        let leaf = self.store.lookup(host_xid);
        if leaf
            .and_then(|id| self.store.redirected_target(id))
            .is_some()
        {
            return None;
        }
        let target = self.resolve_paint_target(host_xid)?;
        if Some(target.backing_id()) == leaf {
            return None;
        }
        Some(SharedBackingMoveSource {
            occluders: self.copy_area_shared_backing_occluders(host_xid, &target),
            target,
        })
    }

    /// Copy what [`Self::shared_backing_move_source`] captured to the
    /// window's new position in the same backing: the window's whole
    /// outer extent, inferiors included, minus higher siblings at the
    /// old position (not the window's pixels) and at the new one (not
    /// the window's to overwrite) — `fbCopyWindow`'s old `borderClip`
    /// intersected with the new one. Whatever the copy cannot cover is
    /// exposed by the core configure path, as in Xorg.
    pub(in crate::kms::render::backend) fn carry_shared_backing_pixels_on_move(
        &mut self,
        host_xid: u32,
        source: SharedBackingMoveSource,
    ) {
        let Some(geom) = self.windows.get(&host_xid).copied() else {
            return;
        };
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return;
        };
        let backing = target.backing_id();
        if backing != source.target.backing_id() {
            return;
        }
        let old_origin = source.target.offset();
        let new_origin = target.offset();
        if old_origin == new_origin {
            return;
        }
        // Window-local CONTENT space, the occluders' frame: the outer
        // extent starts a border width up and left of the content.
        let bw = i32::from(geom.border_width);
        let outer = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: -bw, y: -bw },
            extent: ash::vk::Extent2D {
                width: u32::from(geom.width) + 2 * u32::from(geom.border_width),
                height: u32::from(geom.height) + 2 * u32::from(geom.border_width),
            },
        };
        let new_occluders = self.copy_area_shared_backing_occluders(host_xid, &target);
        // The parent's paint target carries its clip, its own and every
        // ancestor's, in backing coordinates.
        let parent_bounds = geom
            .parent
            .and_then(|p| self.resolve_paint_target(p))
            .filter(|t| t.backing_id() == backing)
            .and_then(PaintTarget::content_bounds);
        let shape = self.bounding_shape_local(host_xid);
        let pieces = shared_backing_move_pieces(
            outer,
            shape.as_deref(),
            &source.occluders,
            &new_occluders,
            parent_bounds,
            old_origin,
            new_origin,
        );
        let delta = (new_origin.0 - old_origin.0, new_origin.1 - old_origin.1);
        for piece in order_pieces_for_in_place_move(pieces, delta) {
            let src_rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: old_origin.0 + piece.offset.x,
                    y: old_origin.1 + piece.offset.y,
                },
                extent: piece.extent,
            };
            let dst_pos = ash::vk::Offset2D {
                x: new_origin.0 + piece.offset.x,
                y: new_origin.1 + piece.offset.y,
            };
            if let Err(e) = self.engine.copy_area(
                &mut self.store,
                &mut self.platform,
                Src::server_internal(backing),
                Dst::server_internal(backing),
                src_rect,
                dst_pos,
            ) {
                log::warn!(
                    "render configure_subwindow: carrying moved window 0x{host_xid:x} \
                     inside its shared backing failed: {e:?}"
                );
                return;
            }
        }
    }

    /// When window branches share one redirected backing, painting a lower
    /// branch must not overwrite higher siblings' visible pixels in that
    /// backing. Walk from the destination through every ancestor: a higher
    /// sibling at any level is an occluder for the whole descendant branch.
    ///
    /// Return those mapped higher-sibling rects in `dst_host_xid` local
    /// coordinates so `copy_area` can subtract them before dispatch. Using
    /// each sibling's resolved backing offset, rather than just its immediate
    /// `(x, y)`, also handles cousins reached through wrapper windows.
    pub(in crate::kms::render::backend) fn copy_area_shared_backing_occluders(
        &self,
        dst_host_xid: u32,
        dst_target: &PaintTarget,
    ) -> Vec<ash::vk::Rect2D> {
        let mut branch_xid = dst_host_xid;
        let mut occluders = Vec::new();
        while let Some(branch_geom) = self.windows.get(&branch_xid) {
            occluders.extend(
                self.windows
                    .iter()
                    .filter_map(|(sibling_host_xid, sibling_geom)| {
                        if *sibling_host_xid == branch_xid
                            || !sibling_geom.mapped
                            || sibling_geom.parent != branch_geom.parent
                            || sibling_geom.stack_rank <= branch_geom.stack_rank
                            || sibling_geom.width == 0
                            || sibling_geom.height == 0
                        {
                            return None;
                        }
                        let sibling_target = self.resolve_paint_target(*sibling_host_xid)?;
                        (sibling_target.backing_id() == dst_target.backing_id()).then_some((
                            sibling_geom.stack_rank,
                            ash::vk::Rect2D {
                                offset: ash::vk::Offset2D {
                                    x: sibling_target.offset().0 - dst_target.offset().0,
                                    y: sibling_target.offset().1 - dst_target.offset().1,
                                },
                                extent: ash::vk::Extent2D {
                                    width: u32::from(sibling_geom.width),
                                    height: u32::from(sibling_geom.height),
                                },
                            },
                        ))
                    }),
            );
            let Some(parent_xid) = branch_geom.parent else {
                break;
            };
            branch_xid = parent_xid;
        }
        occluders.sort_by_key(|(rank, _)| *rank);
        occluders.into_iter().map(|(_, rect)| rect).collect()
    }

    /// `host_xid`'s bounding shape in its own content space, or `None`
    /// when it has none (the shape is then its outer rect).
    fn bounding_shape_local(&self, host_xid: u32) -> Option<Vec<ash::vk::Rect2D>> {
        self.core.shape_bounding.get(&host_xid).map(|rects| {
            rects
                .iter()
                .filter(|r| r.width > 0 && r.height > 0)
                .map(|r| ash::vk::Rect2D {
                    offset: ash::vk::Offset2D {
                        x: i32::from(r.x),
                        y: i32::from(r.y),
                    },
                    extent: ash::vk::Extent2D {
                        width: u32::from(r.width),
                        height: u32::from(r.height),
                    },
                })
                .collect()
        })
    }

    /// What a mapped child takes out of its parent under ClipByChildren,
    /// in the parent's content space: `content_box` for an unshaped child,
    /// else its bounding shape within its outer rect — Xorg subtracts the
    /// child's `borderSize`, which `SetBorderSize` intersects with
    /// `wBoundingShape` (`dix/window.c:1747-1770`). GDK clips a native
    /// window inside a client-side one with exactly such a shape, so the
    /// parent's widgets outside it (a dialog's button bar) stay drawable.
    pub(in crate::kms::render::backend) fn child_clip_region(
        &self,
        child_host_xid: u32,
        geom: &WindowGeometry,
        content_box: ash::vk::Rect2D,
    ) -> Vec<ash::vk::Rect2D> {
        let Some(shape) = self.bounding_shape_local(child_host_xid) else {
            return vec![content_box];
        };
        let bw = i32::from(geom.border_width);
        let (cx, cy) = (i32::from(geom.x) + bw, i32::from(geom.y) + bw);
        let outer = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(geom.x),
                y: i32::from(geom.y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(geom.width) + 2 * u32::from(geom.border_width),
                height: u32::from(geom.height) + 2 * u32::from(geom.border_width),
            },
        };
        let in_parent: Vec<ash::vk::Rect2D> = shape
            .into_iter()
            .map(|r| ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: r.offset.x + cx,
                    y: r.offset.y + cy,
                },
                extent: r.extent,
            })
            .collect();
        intersect_rect_with_clip(outer, &in_parent)
    }

    /// Where `host_xid` may draw in a backing it shares with its
    /// ancestors, beyond the bounds its paint target already carries, in
    /// its own content space: inside its own bounding shape and each
    /// ancestor's up to the backing's owner, and outside every higher
    /// sibling at each of those levels (their pixels share the backing;
    /// [`Self::copy_area_shared_backing_occluders`]). Xorg's clipList
    /// gets the same from `miComputeClips` (`mi/mivaltree.c:390-437`).
    /// `None` when nothing narrows the bounds — always for a window
    /// drawing into storage of its own.
    pub(in crate::kms::render::backend) fn shared_backing_draw_clip(
        &self,
        host_xid: u32,
        target: &PaintTarget,
    ) -> Option<Vec<ash::vk::Rect2D>> {
        let leaf = self.store.lookup(host_xid);
        if Some(target.backing_id()) == leaf
            || leaf
                .and_then(|id| self.store.redirected_target(id))
                .is_some()
            || !self.windows.contains_key(&host_xid)
        {
            return None;
        }
        let mut region: Option<Vec<ash::vk::Rect2D>> = None;
        // The current level's content origin, in `host_xid`'s content space.
        let (mut ox, mut oy) = (0i32, 0i32);
        let mut cur = host_xid;
        loop {
            if let Some(shape) = self.bounding_shape_local(cur) {
                let shape: Vec<ash::vk::Rect2D> = shape
                    .into_iter()
                    .map(|r| ash::vk::Rect2D {
                        offset: ash::vk::Offset2D {
                            x: r.offset.x + ox,
                            y: r.offset.y + oy,
                        },
                        extent: r.extent,
                    })
                    .collect();
                region = Some(match region {
                    None => shape,
                    Some(cur_region) => cur_region
                        .into_iter()
                        .flat_map(|r| intersect_rect_with_clip(r, &shape))
                        .collect(),
                });
            }
            let owner = self
                .store
                .lookup(cur)
                .and_then(|id| self.store.redirected_target(id))
                .is_some();
            let Some(geom) = self.windows.get(&cur) else {
                break;
            };
            let Some(parent) = geom.parent.filter(|_| !owner) else {
                break;
            };
            // `cur`'s content origin sits at `(x + bw, y + bw)` in its
            // parent's content space.
            ox -= i32::from(geom.x) + i32::from(geom.border_width);
            oy -= i32::from(geom.y) + i32::from(geom.border_width);
            cur = parent;
        }
        let occluders = self.copy_area_shared_backing_occluders(host_xid, target);
        if region.is_none() && occluders.is_empty() {
            return None;
        }
        let bounds = target.content_bounds().map_or(
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::MIN / 2,
                    y: i32::MIN / 2,
                },
                extent: ash::vk::Extent2D {
                    width: u32::MAX / 2,
                    height: u32::MAX / 2,
                },
            },
            |b| ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: b.offset.x - target.offset().0,
                    y: b.offset.y - target.offset().1,
                },
                extent: b.extent,
            },
        );
        let region = region.map_or_else(|| vec![bounds], |r| intersect_rect_with_clip(bounds, &r));
        Some(
            region
                .into_iter()
                .flat_map(|r| compute_copy_area_dst_rects(r, &occluders))
                .collect(),
        )
    }
}
