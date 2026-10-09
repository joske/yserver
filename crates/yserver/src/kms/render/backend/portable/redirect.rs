use super::*;

impl KmsBackend {
    /// Stage 4c.2 — compute the screen-absolute rect for a window's
    /// `DrawableId`. Walks the `windows.parent` chain upward
    /// from `w_id`, accumulating each step's `(x, y)` offset; the
    /// resulting rect's `offset` is the window's root-relative
    /// origin and the `extent` is its own `width × height`.
    ///
    /// Returns `None` when:
    /// - `w_id` doesn't resolve in the store, OR
    /// - the leaf xid has no `windows` entry (Pixmap / Root /
    ///   detached), OR
    /// - the parent chain hits a dangling `Some(xid)` that is
    ///   neither root (`core.window_id`) nor a tracked
    ///   `windows` entry. Bailing keeps callers from acting on
    ///   a half-accumulated rect; Stage 5 cache work can revisit
    ///   if the conservative choice ever bites.
    ///
    /// Consumed by Stage 4c.4's `set_window_scene_participation`:
    /// it captures the previous on-screen rect BEFORE flipping
    /// `scene_participating` so it can fire
    /// `mark_scene_structure_damage_rects(&[prev_rect])` for the
    /// redirect transition.
    pub(crate) fn window_absolute_rect(
        &self,
        w_id: crate::kms::render::store::DrawableId,
    ) -> Option<ash::vk::Rect2D> {
        let leaf_xid = self.store.get(w_id)?.xid;
        let leaf_geom = self.windows.get(&leaf_xid)?;
        let mut abs_x = i32::from(leaf_geom.x);
        let mut abs_y = i32::from(leaf_geom.y);
        let mut cur_parent = leaf_geom.parent;
        while let Some(parent_xid) = cur_parent {
            if parent_xid == self.core.window_id {
                // Reached root explicitly — root is the (0, 0)
                // origin of the screen-absolute coordinate space.
                break;
            }
            let Some(parent_geom) = self.windows.get(&parent_xid) else {
                // Dangling parent: not root, not tracked. Bail.
                return None;
            };
            abs_x += i32::from(parent_geom.x);
            abs_y += i32::from(parent_geom.y);
            cur_parent = parent_geom.parent;
        }
        Some(ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: abs_x, y: abs_y },
            extent: ash::vk::Extent2D {
                width: u32::from(leaf_geom.width),
                height: u32::from(leaf_geom.height),
            },
        })
    }

    /// Audit #6 (2026-05-19) — Xorg parity rewrite. The old
    /// "copy W's own storage + DFS-walk descendants" model
    /// (Stage 4b.5) preserved any pre-redirect paint on W, but
    /// freshly-mapped windows that hit redirect activation
    /// before their first paint seeded B with W's default-init
    /// colour (opaque black on depth-24, transparent on
    /// depth-32) — visible as the recurring "black band on map"
    /// symptom and called out by the 2026-05-19 protocol audit.
    ///
    /// Replaced with Xorg's `compNewPixmap`
    /// (composite/compalloc.c:541-606) semantics: copy from
    /// W's PARENT at the source offset
    /// `(W.x - parent.x, W.y - parent.y)` to B at `(0, 0)`.
    /// This gives the compositor / direct-emit scene-walk
    /// continuity with what was on-screen before W appeared.
    /// The W's own (default-init) content is not preserved;
    /// W's first client paint fills B via `resolve_paint_target`
    /// routing afterwards.
    ///
    /// Skipped when:
    /// - W has no parent in `windows` (root or untracked).
    /// - Parent's storage isn't in the store (pixmap-as-W
    ///   activation path used by tests; falls back to leaving
    ///   B at its default-init zero-fill).
    /// - Parent's storage extent is zero.
    ///
    /// `IncludeInferiors`-equivalent semantics (parent's siblings
    /// of W contributing where they overlap W's screen position)
    /// are deferred: yserver's parent storage already includes
    /// most of that content via the normal paint flow for
    /// non-compositor cases, and the Stage 4d compositor-floor
    /// scene-walk presents siblings directly.
    pub(in crate::kms::render::backend) fn seed_backing_from_parent(
        &mut self,
        w_xid: u32,
        b_id: crate::kms::render::store::DrawableId,
    ) {
        let Some(w_geom) = self.windows.get(&w_xid).copied() else {
            log::debug!("render seed_backing_from_parent W=0x{w_xid:x}: not in windows; skip seed");
            return;
        };
        let Some(parent_xid) = w_geom.parent else {
            log::debug!(
                "render seed_backing_from_parent W=0x{w_xid:x}: no parent (root or untracked); skip seed"
            );
            return;
        };
        // Resolve parent's effective storage. If parent is itself
        // redirected, its `redirected_target` (B') holds the
        // currently-visible pixels; if not, parent's own storage
        // does. `resolve_paint_target` does the chain walk.
        let Some(parent_target) = self.resolve_paint_target(parent_xid) else {
            log::debug!(
                "render seed_backing_from_parent W=0x{w_xid:x}: parent 0x{parent_xid:x} has no paint target; skip seed"
            );
            return;
        };
        let parent_extent = self
            .store
            .get(parent_target.backing_id())
            .map(|d| d.storage.extent)
            .unwrap_or_default();
        if parent_extent.width == 0 || parent_extent.height == 0 {
            log::debug!(
                "render seed_backing_from_parent W=0x{w_xid:x}: parent storage zero-extent; skip seed"
            );
            return;
        }
        // Source rect on parent: W's OUTER origin in the parent's
        // drawable space. `parent_target.offset()` is the parent's
        // content origin inside the storage the copy reads (its own, or
        // an ancestor backing when the parent is itself redirected);
        // W's wire `(x, y)` is its OUTER origin relative to that
        // content origin (`dix/window.c` sets
        // `drawable.x = parent->drawable.x + x + bw`).
        //
        // #133 step 3 (3.3): B is now the BORDERED extent placed at W's
        // outer origin, so the seed must cover `w + 2bw` x `h + 2bw`
        // and land at B's `(0, 0)` — exactly Xorg
        // `compNewPixmap(pWin, x, y, w, h)` with
        // `x = drawable.x - bw` / `w = width + (bw << 1)`
        // (`composite/compalloc.c:608-618`) copying
        // `CopyArea(parent → pixmap, x - pParent->drawable.x,
        // y - pParent->drawable.y, w, h, 0, 0)`
        // (`composite/compalloc.c:566-571`). Seeding only `w x h` here
        // would put the parent's pixels in the ring and shift the whole
        // inherited image by `bw`. Collapses to the pre-#133 rect at
        // `bw == 0`.
        let src_x = parent_target.offset().0 + i32::from(w_geom.x);
        let src_y = parent_target.offset().1 + i32::from(w_geom.y);
        let (outer_w, outer_h) =
            bordered_storage_extent(w_geom.width, w_geom.height, w_geom.border_width);
        let src_rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: src_x.max(0),
                y: src_y.max(0),
            },
            extent: ash::vk::Extent2D {
                width: outer_w,
                height: outer_h,
            },
        };
        let dst_pos = ash::vk::Offset2D { x: 0, y: 0 };
        log::debug!(
            "render seed_backing_from_parent W=0x{w_xid:x} parent=0x{parent_xid:x} \
             src=({src_x},{src_y} {outer_w}x{outer_h} outer, bw={bw}) → B@(0,0)",
            bw = w_geom.border_width,
        );
        // Server-internal redirect seeding: B is initialised from the
        // parent's backing in BACKING space (both handles privileged).
        if let Err(e) = self.engine.copy_area(
            &mut self.store,
            &mut self.platform,
            parent_target.server_backing_src(),
            Dst::server_internal(b_id),
            src_rect,
            dst_pos,
        ) {
            log::warn!(
                "render seed_backing_from_parent(0x{w_xid:x}): parent copy_area failed: {e:?}",
            );
        } else {
            self.telemetry.record_paint_submit();
            self.trace_simple(SubmitKind::CopyArea, b_id, 1);
        }
    }

    /// 2026-06-11 — synthesize Xorg `compNewPixmap`'s
    /// `IncludeInferiors` over a freshly-allocated redirect backing.
    ///
    /// `seed_backing_from_parent` lays down the parent layer — Xorg's
    /// `CopyArea(parent, …, IncludeInferiors)` base
    /// (../xserver/composite/compalloc.c:562). But Xorg has NO separate
    /// per-window storage, so that copy captures the redirected
    /// window's OWN pixels (it is an inferior of the parent). yserver
    /// keeps a separate per-window leaf, so the parent storage holds
    /// only the parent's pixels (wallpaper for a root child) and W's
    /// own content is lost — the "half-drawn MATE panel under compiz
    /// --replace" bug (static regions never repaint after a
    /// compositor handoff, so they keep the wallpaper seed).
    ///
    /// This reconstructs the inferiors: DFS over W and its mapped
    /// descendants in `stack_rank` (bottom-to-top) order, compositing
    /// each window's own leaf into B at its accumulated offset from W.
    /// A descendant that owns its OWN `redirected_target` (an
    /// independently-redirected child — XEmbed systray icons, etc.) is
    /// PRUNED with its whole subtree: the client compositor reads and
    /// composites that child's backing separately via
    /// `NameWindowPixmap`, so its pixels must not be baked into W's
    /// backing.
    ///
    /// Must run BEFORE `set_redirected_target(W)` so the walk reads
    /// each window's leaf, not the about-to-be-installed route. Keep
    /// the per-window/stacking rules in sync with
    /// `scene::emit_window_subtree` (scene.rs:2406).
    pub(in crate::kms::render::backend) fn overlay_backing_inferiors(
        &mut self,
        w_xid: u32,
        b_id: crate::kms::render::store::DrawableId,
    ) {
        use crate::kms::{
            cpu_types::Repeat,
            render::engine::{ResolvedSource, SourceDrawable},
            vk::ops::render::CompositeRect,
        };
        let plan = self.plan_backing_inferiors(w_xid, b_id);
        if plan.is_empty() {
            return;
        }
        // RAW COPY, not a composite. Xorg's `compNewPixmap` seeds the
        // new backing with `CopyArea(parent, …, IncludeInferiors)`
        // (`composite/compalloc.c:562`), and CopyArea replaces the
        // destination — it has no notion of alpha at all. This walk
        // exists only because yserver keeps a separate per-window leaf,
        // so it must reproduce the same REPLACEMENT that a single
        // shared-storage CopyArea would have performed, one leaf at a
        // time in bottom-to-top stack order.
        //
        // This was `PictOpOver` until 2026-09-11, on the reasoning that
        // "alpha children blend over the parent base". They must not: a
        // depth-32 child with α = 0 then blends as a NO-OP and the
        // parent's seed shows through its background. Measured — a
        // window asking for `background-pixel = 0x00000000` kept the
        // root's red and read back `00ff0000` where Xorg 21.1.24 gives
        // `00000000` (`tools/depth32-bg-probe.c`, transparent-zero
        // A-root, vng 2026-09-11).
        //
        // The same probe shows the replacement model is what Xorg
        // actually stores: on 21.1.24 a depth-24 frame's redirected
        // pixmap reads `00ffffff` under an ARGB child whose background
        // is transparent white — the child's own bits, alpha and all,
        // stamped into the parent's pixmap rather than blended with it.
        //
        // Shape and stacking are unaffected: `push_inferior_rects`
        // already intersects each leaf with its `shape_bounding` and
        // `collect_backing_inferiors` walks in `stack_rank` order, so
        // only the blend equation changes here. Depth-24 leaves still
        // land opaque — `sample_view` gives them α = 1 — so the one
        // behaviour that changes is the one that was wrong.
        const OP_SRC: u8 = 1;
        for d in plan {
            // Skip leaves whose storage has no realized view (no GPU
            // backing yet) — the planner left the liveness check here.
            if self
                .store
                .get(d.leaf_id)
                .is_none_or(|s| s.storage.image_view == ash::vk::ImageView::null())
            {
                continue;
            }
            let rects = [CompositeRect {
                src_x: d.src_x,
                src_y: d.src_y,
                mask_x: 0,
                mask_y: 0,
                dst_x: d.dst_x,
                dst_y: d.dst_y,
                width: d.width,
                height: d.height,
            }];
            match self.engine.render_composite(
                &mut self.store,
                &mut self.platform,
                OP_SRC,
                // The planner already put this leaf's CONTENT origin
                // into `src_x`/`src_y` (`push_inferior_rects`), so the
                // source is addressed in raw leaf-storage coordinates
                // here — `whole` is correct and `content` would
                // double-count the border.
                ResolvedSource::Drawable(SourceDrawable::whole(d.leaf_id)),
                ResolvedSource::None,
                Dst::server_internal(b_id),
                &rects,
                None,
                Repeat::None,
                Repeat::None,
                None,
                None,
                false,
                // Synthesized seed; no Picture context — engine falls
                // back to the depth heuristic (→ `sample_view`).
                0,
                0,
                0,
            ) {
                Ok(s) if s.recorded_draws > 0 => {
                    self.telemetry.record_paint_submit();
                    self.trace_simple(SubmitKind::RenderComposite, b_id, s.recorded_draws);
                }
                Ok(_) => {}
                Err(e) => log::warn!(
                    "render overlay_backing_inferiors(0x{w_xid:x}): leaf {leaf:?} composite failed: {e:?}",
                    leaf = d.leaf_id,
                ),
            }
        }
    }

    /// Pure planner for [`Self::overlay_backing_inferiors`] — returns
    /// the ordered leaf→backing composites (backing-local coords,
    /// B's (0,0) == W's origin). Split out so the
    /// traversal/prune/clip logic is unit-testable without a live
    /// `RenderEngine`/Vulkan context.
    pub(in crate::kms::render::backend) fn plan_backing_inferiors(
        &self,
        w_xid: u32,
        b_id: crate::kms::render::store::DrawableId,
    ) -> Vec<SeedInferiorDraw> {
        let mut out = Vec::new();
        let b_extent = self
            .store
            .get(b_id)
            .map_or(ash::vk::Extent2D::default(), |d| d.storage.extent);
        if b_extent.width == 0 || b_extent.height == 0 {
            return out;
        }
        // #133 step 3 (3.3): B's `(0, 0)` is W's OUTER origin now, so
        // the walk starts at W's CONTENT origin inside B — `(bw, bw)`,
        // Xorg's `compSetPixmap(pWin, pPixmap, bw)`
        // (`composite/compalloc.c:620`). `(0, 0)` at `bw == 0`.
        let seed_bw = self.window_border_width(w_xid);
        self.collect_backing_inferiors(w_xid, seed_bw, seed_bw, true, &mut out);
        out
    }

    /// Recursive worker for [`Self::plan_backing_inferiors`].
    /// `(off_x, off_y)` is the current window's CONTENT origin in
    /// backing-local coords (#133 step 3: B's `(0, 0)` is W's OUTER
    /// origin, and each level adds `child.x + child.border_width` —
    /// the same recurrence `resolve_window_paint_target` walks);
    /// `is_seed_root` is true only for W itself (W is becoming
    /// redirected now, so its own `redirected_target` must not prune
    /// it).
    fn collect_backing_inferiors(
        &self,
        xid: u32,
        off_x: i32,
        off_y: i32,
        is_seed_root: bool,
        out: &mut Vec<SeedInferiorDraw>,
    ) {
        let Some(geom) = self.windows.get(&xid).copied() else {
            return;
        };
        if !geom.mapped {
            // X11: an unmapped window (and its whole subtree) is invisible.
            return;
        }
        let Some(leaf_id) = self.store.lookup(xid) else {
            return;
        };

        // Prune at an independently-redirected descendant: its backing
        // is composited separately by the client compositor, so its
        // pixels (and its subtree) must not be baked into W's backing.
        if !is_seed_root && self.store.redirected_target(leaf_id).is_some() {
            return;
        }

        // Plan this window's own leaf if it's a Window with sized
        // storage. The GPU-liveness check (`image_view != null`) is
        // deferred to the consumer so this planner stays pure/testable.
        if let Some(d) = self.store.get(leaf_id)
            && matches!(d.kind, crate::kms::render::store::DrawableKind::Window)
        {
            // #133 step 3: the leaf's CONTENT is what composites into
            // B, and it lives `bw` inside the leaf's own storage
            // (`compAllocPixmap`, `composite/compalloc.c:610`), so the
            // clamp is against the storage minus the ring and the
            // source origin is `(bw, bw)`. Identity at `bw == 0`.
            let bw = u32::from(geom.border_width);
            let content_cap_w = d.storage.extent.width.saturating_sub(bw.saturating_mul(2));
            let content_cap_h = d.storage.extent.height.saturating_sub(bw.saturating_mul(2));
            let w = u32::from(geom.width).min(content_cap_w);
            let h = u32::from(geom.height).min(content_cap_h);
            self.push_inferior_rects(xid, leaf_id, off_x, off_y, w, h, out);
        }

        // Recurse mapped children bottom-to-top, same `stack_rank`
        // order as `scene::emit_window_subtree` (scene.rs:2406).
        let mut children: Vec<(u32, u64)> = self
            .windows
            .iter()
            .filter_map(|(c, g)| (g.parent == Some(xid)).then_some((*c, g.stack_rank)))
            .collect();
        children.sort_by_key(|(_, rank)| *rank);
        for (child, _) in children {
            // `off` is the PARENT's content origin; a child's outer
            // origin is `+ (x, y)` from there and its own content
            // origin `+ bw` inside that (#133 step 3 — the same
            // one-level translation `resolve_window_paint_target` uses,
            // `W.border_width + C.x + C.border_width`).
            let (cx, cy, cbw) = self.windows.get(&child).map_or((0, 0, 0), |g| {
                (i32::from(g.x), i32::from(g.y), i32::from(g.border_width))
            });
            self.collect_backing_inferiors(child, off_x + cx + cbw, off_y + cy + cbw, false, out);
        }
    }

    /// Emit the leaf→backing rects for one window, honoring SHAPE
    /// bounding (one rect per bound, mirroring scene.rs:2302) and
    /// clamping negative destination offsets by shifting the source
    /// (the off-parent/offscreen case the parent-seed copy mishandled).
    ///
    /// #133 step 3: `(off_x, off_y)` is the window's CONTENT origin in
    /// backing-local coords, and the SOURCE origin is the window's
    /// content origin inside its own leaf storage — `(bw, bw)`, not
    /// `(0, 0)`. SHAPE bounds are window-relative (content space), so
    /// they are offset by `bw` on the source side only.
    fn push_inferior_rects(
        &self,
        xid: u32,
        leaf_id: crate::kms::render::store::DrawableId,
        off_x: i32,
        off_y: i32,
        w: u32,
        h: u32,
        out: &mut Vec<SeedInferiorDraw>,
    ) {
        if w == 0 || h == 0 {
            return;
        }
        let src_bw = self.window_border_width(xid);
        let mut emit = |sx: i32, sy: i32, dx: i32, dy: i32, rw: i32, rh: i32| {
            let (mut sx, mut sy, mut dx, mut dy) = (sx, sy, dx, dy);
            let (mut rw, mut rh) = (i64::from(rw), i64::from(rh));
            // Low-side clamp: a negative dst shifts the source and
            // shrinks the rect. High-side clipping is left to
            // `render_composite`'s dst-extent clamp.
            if dx < 0 {
                sx -= dx;
                rw += i64::from(dx);
                dx = 0;
            }
            if dy < 0 {
                sy -= dy;
                rh += i64::from(dy);
                dy = 0;
            }
            if rw <= 0 || rh <= 0 {
                return;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            out.push(SeedInferiorDraw {
                leaf_id,
                src_x: sx,
                src_y: sy,
                dst_x: dx,
                dst_y: dy,
                width: rw as u32,
                height: rh as u32,
            });
        };
        if let Some(rects) = self.core.shape_bounding.get(&xid) {
            for r in rects {
                let (rx, ry) = (i32::from(r.x), i32::from(r.y));
                let (rw, rh) = (i32::from(r.width), i32::from(r.height));
                // Intersect the bound with the window box [0,w)×[0,h).
                let cx = rx.max(0);
                let cy = ry.max(0);
                #[allow(clippy::cast_possible_wrap)]
                let cw = (rx + rw).min(w as i32) - cx;
                #[allow(clippy::cast_possible_wrap)]
                let ch = (ry + rh).min(h as i32) - cy;
                if cw <= 0 || ch <= 0 {
                    continue;
                }
                emit(cx + src_bw, cy + src_bw, off_x + cx, off_y + cy, cw, ch);
            }
        } else {
            #[allow(clippy::cast_possible_wrap)]
            emit(src_bw, src_bw, off_x, off_y, w as i32, h as i32);
        }
    }

    /// Bridge `store.decref` to `engine.notify_drawable_retired`.
    /// When `decref` decides the drawable is destroyable, the
    /// closure fires BEFORE `Storage::destroy`, so any
    /// `VkImageView` cached in `RenderEngine::drawable_view_cache`
    /// for that `DrawableId` is destroyed while its underlying
    /// `VkImage` is still alive. Without this hook the cache
    /// accumulates entries pointing at freed images for the
    /// lifetime of the session (only swept at engine `Drop`).
    /// `DrawableId`s are monotonically allocated so stale entries
    /// can never alias a fresh drawable — the leak is memory-only,
    /// not use-after-free — but unbounded growth still matters.
    pub(crate) fn store_decref_with_invalidate(
        &mut self,
        id: crate::kms::render::store::DrawableId,
    ) -> crate::kms::render::store::RetireDecision {
        let engine = &mut self.engine;
        let scanout_m1 = &mut self.scanout_m1;
        self.store.decref(&mut self.platform, id, |dropped| {
            engine.notify_drawable_retired(dropped);
            scanout_m1.remove(dropped);
        })
    }

    /// Allocate a fresh store entry and apply any deferred picture
    /// refs that were waiting for this xid's backing to materialize.
    /// Every production backing-materialization path goes through
    /// this so pictures created before the backing existed still pin
    /// it (see [`Self::apply_pending_picture_refs`]).
    pub(in crate::kms::render::backend) fn store_alloc(
        &mut self,
        xid: u32,
        kind: DrawableKind,
        depth: u8,
        scene_participating: bool,
        storage: Storage,
    ) -> Result<DrawableId, AllocError> {
        let id = self
            .store
            .allocate(xid, kind, depth, scene_participating, storage)?;
        self.apply_pending_picture_refs(xid);
        Ok(id)
    }

    /// Apply the deferred store refs for pictures that wrapped
    /// `host_xid` before its backing existed. Called after every
    /// successful materialization (`store_alloc`). A picture created
    /// early (window map + redirect before backing alloc, GLX-TFP /
    /// Present / DRI3 import) must still pin the backing once it
    /// appears, or a later `free_pixmap` reaches refcount 0 and
    /// destroys the drawable out from under the live Picture —
    /// the game-start transparency bug.
    pub(crate) fn apply_pending_picture_refs(&mut self, host_xid: u32) {
        let pending: Vec<u32> = self
            .pending_picture_drawable_refs
            .iter()
            .filter(|(_, x)| **x == host_xid)
            .map(|(pic, _)| *pic)
            .collect();
        if pending.is_empty() {
            return;
        }
        let Some(id) = self.store.lookup(host_xid) else {
            return;
        };
        for pic in pending {
            self.pending_picture_drawable_refs.remove(&pic);
            self.store.incref(id);
            self.picture_drawable_ids.insert(pic, id);
        }
    }

    /// Take ONE lifetime ref on the backing's owning counter so the
    /// storage survives an early `FreePixmap` while a GL consumer still
    /// references it. Returns `true` if the ref was taken on
    /// `alias_registry` (NameWindowPixmap'd redirect backing), `false`
    /// if on the `DrawableStore` refcount (plain pixmap).
    pub(in crate::kms::render::backend) fn take_backing_lifetime_ref(
        &mut self,
        id: crate::kms::render::store::DrawableId,
        backing: PixmapHandle,
    ) -> bool {
        if self.core.alias_registry.get(backing).is_some() {
            self.core.alias_registry.incref(backing);
            true
        } else {
            self.store.incref(id);
            false
        }
    }

    /// Release the single lifetime ref taken in `take_backing_lifetime_ref`.
    /// Routes to the counter recorded at take time. For the alias path a
    /// final decref frees the backing via the same release path
    /// `free_pixmap` uses; for the plain path the store decref frees the
    /// storage when its own refcount also hits zero.
    pub(in crate::kms::render::backend) fn release_backing_lifetime_ref(
        &mut self,
        entry: &ExportedBacking,
    ) {
        if entry.lifetime_via_alias {
            // Alias path: decref the registry (keyed by xid); on final ref
            // free the backing via the same store decref `free_pixmap` uses.
            if self.core.alias_registry.decref(entry.backing) {
                self.store_decref_with_invalidate(entry.backing_id);
            }
        } else {
            // Plain path: drop the extra store ref taken at acquire time.
            // Use the captured DrawableId — the `by_xid` mapping may have
            // been detached by an intervening FreePixmap (PendingFence).
            self.store_decref_with_invalidate(entry.backing_id);
        }
    }

    /// Bridge `store.poll_pending_retire` to
    /// `engine.notify_drawable_retired`. Per the rationale on
    /// `store_decref_with_invalidate`, drawables that were
    /// parked in `pending_retire` (waiting for their fence to
    /// signal) get their engine-side view caches dropped before
    /// `Storage::destroy` runs.
    pub(crate) fn poll_pending_retire_with_invalidate(&mut self) {
        let engine = &mut self.engine;
        let scanout_m1 = &mut self.scanout_m1;
        self.store
            .poll_pending_retire(&mut self.platform, |dropped| {
                engine.notify_drawable_retired(dropped);
                scanout_m1.remove(dropped);
            });
    }
}

impl KmsBackend {
    /// Stage 4c.4 — flip a window's scene-participation under
    /// COMPOSITE redirect. Delegates to `DrawableStore::
    /// set_scene_participating` (which clears unpresented
    /// presentation damage + bumps the epoch on a true→false
    /// transition per spec §I5) and fires scene-structure damage
    /// for the redirect transition.
    ///
    /// **Scene-structure damage** — always fires per the plan's
    /// Cross-cutting §"Concrete scene-structure damage":
    ///   - `participating=true` (un-redirect / Automatic-activate):
    ///     rect = W's current screen rect — the scene newly
    ///     includes W and must paint W's location.
    ///   - `participating=false` (Manual-activate): rect = W's
    ///     pre-flip rect — the scene NO LONGER includes W but
    ///     whatever is underneath must repaint the area where W
    ///     used to be.
    ///
    /// In both branches we capture the rect BEFORE the flip
    /// (pre-flip and post-flip geometry coincide because the
    /// participation flip itself doesn't move W); the only
    /// difference is semantic. When `window_absolute_rect`
    /// returns `None` (root or untracked geometry), fall back to
    /// the coarse `mark_scene_structure_dirty` — correctness-
    /// preserving, just wider than needed.
    pub(in crate::kms::render::backend) fn backend_redirect_set_window_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
        participating: bool,
    ) -> io::Result<()> {
        let Some(w_id) = self.store.lookup(host_window.as_raw()) else {
            log::debug!(
                "render set_window_scene_participation(0x{:x}, {participating}): \
                 window not in store",
                host_window.as_raw(),
            );
            return Ok(());
        };
        // Capture rect BEFORE the flip — on participating=false
        // (Manual activation) the pre-flip rect is what the scene
        // needs to repaint over; on participating=true the pre-
        // and post-flip rects coincide (no geometry move on this
        // path) so either reading is fine, and pre-flip keeps the
        // two branches symmetric.
        let pre_flip_rect = self.window_absolute_rect(w_id);

        self.store.set_scene_participating(w_id, participating);

        if let Some(rect) = pre_flip_rect {
            self.scene.mark_scene_structure_damage_rects(&[rect]);
        } else {
            // No tracked geometry (root or untracked) — coarse
            // marker is correctness-preserving.
            self.scene.mark_scene_structure_dirty();
        }
        Ok(())
    }

    /// Stage 4c.4 — flip a backing's scene-participation under
    /// COMPOSITE redirect. Used by Automatic mode so paint that
    /// resolves through the backing accumulates presentation
    /// damage on B (which the scene walk picks up via W's
    /// `redirected_target` indirection in 4c's `build_scene`
    /// patch). No scene-structure damage from this call — the
    /// geometric damage of a mode-flip is the W-side call's
    /// responsibility (the blit-source identity flip is
    /// geometrically on W; backings have no on-screen geometry
    /// of their own).
    pub(in crate::kms::render::backend) fn backend_redirect_set_backing_scene_participation(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
        participating: bool,
    ) -> io::Result<()> {
        let Some(b_id) = self.store.lookup(backing.as_raw()) else {
            log::debug!(
                "render set_backing_scene_participation(0x{:x}, {participating}): \
                 backing not in store",
                backing.as_raw(),
            );
            return Ok(());
        };
        self.store.set_scene_participating(b_id, participating);
        // No wake: a backing is a pixmap, never a walked node, so its
        // `scene_participating` is not a `decide_node` input — it only gates
        // whether the backing's own presentation damage is peeked/pending.
        // The WINDOW's flag is what the walk reads, and
        // `set_window_scene_participation` dirties for that. Pinned by
        // `set_backing_scene_participation_flips_flag_no_damage`.
        Ok(())
    }

    /// Stage 4b: real `name_window_pixmap`. Mirrors v1
    /// (`kms/backend.rs:9523-9544`) — lookup `host_window_to_backing`,
    /// incref the alias registry, return the SAME handle.
    /// Returns `NotFound` if the window isn't redirected
    /// (`allocate_redirected_backing` was never called for it).
    pub(in crate::kms::render::backend) fn backend_redirect_name_window_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        let backing = self
            .core
            .host_window_to_backing
            .get(&host_window.as_raw())
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "render name_window_pixmap: window is not redirected (no backing)",
                )
            })?;
        self.core.alias_registry.incref(backing);
        Ok(backing)
    }

    /// Stage 4b: real `allocate_redirected_backing`. Mirrors v1
    /// (`kms/backend.rs:9568-9607`) with one v2-specific addition:
    /// after allocating the backing and registering it in
    /// `alias_registry` + `host_window_to_backing`, also flip
    /// `store.set_redirected_target(W_id, Some(B_id))` so v2's
    /// `resolve_paint_target` routes future paint to the backing.
    ///
    /// **Seed-copy ordering** per the plan's Cross-cutting
    /// §"Initial backing content" decision: the W→B copy fires
    /// BEFORE `set_redirected_target` flips routing, so the copy
    /// reads from W's own storage (not B's). Descendant seed-copy
    /// follows the same one-shot walk, in stable sibling z-order,
    /// so overlapping frame/decor children seed into the backing in
    /// the same order they would appear on screen.
    pub(in crate::kms::render::backend) fn backend_redirect_allocate_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<PixmapHandle> {
        // Idempotent — second `RedirectWindow` for the same W
        // returns the existing backing with no refcount bump
        // (the Reason-1 hold is single-instance per
        // §"Single refcount, two reasons").
        if let Some(existing) = self
            .core
            .host_window_to_backing
            .get(&host_window.as_raw())
            .copied()
        {
            return Ok(existing);
        }
        let w_xid = host_window.as_raw();

        // Allocate a fresh backing via the existing
        // `create_pixmap` path (3f.10 pool + 3f.14 zero-fill).
        let backing = self.create_pixmap(origin, depth, width, height)?;
        let backing_xid = backing.as_raw();

        // Seed-copy: parent → B at W's position, BEFORE the route
        // flip. Audit #6 (2026-05-19) flipped this from "W → B (+
        // descendants)" to "parent → B" per Xorg's compNewPixmap
        // (composite/compalloc.c:541-606). Parent's resolve_paint_target
        // gives the storage holding parent's currently-visible pixels
        // (parent's own storage if non-redirected, parent's B if
        // chain-redirected). W's own pre-redirect storage is now
        // ignored — its content was default-init for the
        // newly-mapped-W case (the "black band on map" symptom) and
        // any pre-paint content lands in B post-flip via the normal
        // resolve_paint_target routing on the next client paint.
        if let Some(b_id) = self.store.lookup(backing_xid) {
            if let Some(d) = self.store.get(b_id) {
                crate::kms::vk::mem_accounting::recategorise(
                    d.storage.memory,
                    crate::kms::vk::mem_accounting::MemCategory::RedirectBacking,
                );
            }
            // #133 step 3 — the backing is allocated at the BORDERED
            // extent by the core (`bordered_backing_extent`,
            // `process_request.rs`), so its content starts `bw` inside
            // it (`compSetPixmap(pWin, pPixmap, bw)`,
            // `composite/compalloc.c:620`). Record that layout before
            // seeding, so the seed and every later paint agree.
            self.store
                .set_content_offset(b_id, self.window_border_width(w_xid));
            self.seed_backing_from_parent(w_xid, b_id);
            // 2026-06-11 — synthesize Xorg compNewPixmap's
            // IncludeInferiors: composite W's own leaf + its
            // (non-independently-redirected) descendants over the
            // parent base, so an already-painted window that won't
            // repaint (compositor handoff) keeps its content instead
            // of the wallpaper seed. MUST run before the route flip.
            self.overlay_backing_inferiors(w_xid, b_id);
            // Now flip routing — after this, paint against W
            // resolves to B via `resolve_paint_target`. The w_id
            // lookup must still succeed; if not, the redirect
            // record stays uninstalled (protocol error upstream).
            if let Some(w_id) = self.store.lookup(w_xid) {
                self.store.set_redirected_target(w_id, Some(b_id));
                // The route flip changes what the walk samples for W (the
                // backing's view, extent and UV denominator). The content is
                // the same, but the tick only walks when told something
                // changed, and the presence signature must catch up.
                self.scene.wake_for_damage();
                // #133 step 4 (4.4) — the ring lives INSIDE the storage
                // and this is a new storage, so it has to be painted
                // into the backing. Xorg does the same on the same
                // event: `compSetPixmapVisitWindow` queues
                // `compRepaintBorder` whenever it hands a window a new
                // pixmap with `bw != 0`
                // (`composite/compwindow.c:137-139`). Must run AFTER
                // the route flip so `resolve_paint_target` returns the
                // backing. No-op at `bw == 0`.
                let tile_origin = self.border_tile_origin(w_xid);
                let _ = self.paint_window_border(w_xid, tile_origin);
            } else {
                // No leaf means not viewable; core realizes the backing after the leaf.
                log::debug!(
                    "render allocate_redirected_backing(0x{w_xid:x}): window has no leaf \
                     (route flip skipped)",
                );
            }
        } else {
            log::warn!(
                "render allocate_redirected_backing(0x{w_xid:x}): backing not in store \
                 (seed + route flip skipped)",
            );
        }

        // Register Reason-1 hold + redirect map. Identical to v1.
        self.core.alias_registry.insert(
            backing,
            crate::kms::core::AliasEntry {
                refcount: 1,
                width,
                height,
                depth,
            },
        );
        self.core.host_window_to_backing.insert(w_xid, backing);
        // Step 2c — the window now samples the backing instead of its own
        // storage, which the scene diff reads as a signature change. Same
        // reasoning as the release path: schedule a tick so that damage is not
        // waiting on an unrelated event.
        self.scene.wake_for_damage();
        Ok(backing)
    }

    /// Stage 4b: real `release_redirected_backing`. Mirrors v1
    /// (`kms/backend.rs:9547-9566`) — clear the
    /// `host_window_to_backing` entry, drop the Reason-1 hold,
    /// free pixmap on refcount=0.
    ///
    /// v2-specific addition: when the redirect map clears, also
    /// drop `store.set_redirected_target` for every window that
    /// was routed through this backing. Multiple windows can
    /// alias the same backing only via NameWindowPixmap (which
    /// is the alias-handle, not a separate redirect), but the
    /// loop is cheap and matches the plan's defensive contract.
    ///
    /// Stage 4c.4 round-3 finding: drop B's `scene_participating`
    /// flag internally so the protocol handler (RedirectWindow
    /// unredirect / destroy path) doesn't need a separate
    /// `set_backing_scene_participation(false)` call. The trait
    /// docstring is the canonical statement of this contract.
    pub(in crate::kms::render::backend) fn backend_redirect_retain_backing_storage(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        // Bump alias_registry refcount. The rotate path pairs this
        // with `drop_backing_storage` around the release→copy gap
        // so the no-alias case doesn't free OLD before the copy
        // sources from it. A miss (`alias_registry.get` returns
        // None) means the backing wasn't tracked here — log and
        // pass through; the caller's later copy will hit the
        // unknown-xid path with its own diagnostic.
        if self.core.alias_registry.get(backing).is_some() {
            self.core.alias_registry.incref(backing);
        } else {
            log::warn!(
                "render retain_backing_storage: 0x{:x} not in alias_registry — no-op",
                backing.as_raw(),
            );
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_redirect_redirected_backing_can_fit(
        &self,
        backing: PixmapHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> bool {
        let Some(id) = self.store.lookup(backing.as_raw()) else {
            return false;
        };
        let Some(drawable) = self.store.get(id) else {
            return false;
        };
        // #143: EXACT, not a high-water mark. `width`/`height` are the
        // bordered extent the backing must have (`bordered_backing_extent`,
        // `process_request.rs`), and Xorg reallocates on inequality in
        // EITHER direction — `compReallocPixmap` compares
        // `pix_w != pOld->drawable.width || pix_h != pOld->drawable.height`
        // (../xserver/composite/compalloc.c:698) with
        // `pix_w = w + (bw << 1)`.
        //
        // A `>=` here made a SHRINK keep the oversized storage while the
        // caller rewrote only the logical geometry: the ring then sat at
        // the OLD content width (measured under awesome+picom, backing
        // 1282x708 with the 2-px ring at columns 1280..1281 for a window
        // that had shrunk to 1276 wide) and nothing re-seeded the backing,
        // leaving an alpha-0 band where content should be.
        drawable.depth == depth
            && drawable.storage.extent.width == u32::from(width)
            && drawable.storage.extent.height == u32::from(height)
    }

    pub(in crate::kms::render::backend) fn backend_redirect_update_redirected_backing_geometry(
        &mut self,
        _origin: Option<OriginContext>,
        backing: PixmapHandle,
        width: u16,
        height: u16,
        depth: u8,
    ) -> io::Result<()> {
        self.core
            .alias_registry
            .update_geometry(backing, width, height, depth);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_redirect_drop_backing_storage(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        // Symmetric to `retain_backing_storage`. Decref the
        // alias_registry; if this was the final ref, free the
        // underlying pixmap. Mirrors the alias-aware branch of
        // `free_pixmap` for consistency.
        let export_id = self.store.lookup(backing.as_raw());
        if self.core.alias_registry.decref(backing) {
            self.free_pixmap(origin, backing.as_raw())?;
        } else if let Some(id) = export_id {
            // An alias left this backing: an export-only entry (glx_refs == 0) goes, as at FreePixmap.
            self.maybe_teardown_export(id);
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_redirect_release_window_pixmap_name(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        // A name owns exactly one alias ref; an untracked backing has none left to drop.
        if self.core.alias_registry.get(backing).is_none() {
            log::warn!(
                "render release_window_pixmap_name: 0x{:x} not in alias_registry — no-op",
                backing.as_raw(),
            );
            return Ok(());
        }
        self.drop_backing_storage(origin, backing)
    }

    pub(in crate::kms::render::backend) fn backend_redirect_release_redirected_backing(
        &mut self,
        origin: Option<OriginContext>,
        backing: PixmapHandle,
    ) -> io::Result<()> {
        let raw = backing.as_raw();
        // Drop the W→B map entry. v1 uses `retain` because the
        // map is keyed by W_xid (not B_xid); same shape here.
        self.core
            .host_window_to_backing
            .retain(|_, h| h.as_raw() != raw);
        // Clear the store-side route on every window that pointed
        // at this backing's DrawableId. Reverse-scan over `entries`
        // would need an iter accessor we don't have; iterate the
        // map keys we just retained against and clear each.
        // (In practice the map is empty after `retain` above —
        // but a future multi-window-per-backing model would still
        // be correct.)
        if let Some(b_id) = self.store.lookup(raw) {
            let routed_windows: Vec<u32> = self
                .windows
                .keys()
                .copied()
                .filter(|xid| {
                    self.store
                        .lookup(*xid)
                        .and_then(|id| self.store.redirected_target(id))
                        == Some(b_id)
                })
                .collect();
            for w_xid in routed_windows {
                if let Some(w_id) = self.store.lookup(w_xid) {
                    self.store.set_redirected_target(w_id, None);
                }
                self.sync_window_leaf_storage_to_geometry(w_xid);
                // ...which just re-initialised the leaf from the
                // background. Put the compositor's content back on top
                // of it, BEFORE the `decref`/`free_pixmap` below can
                // retire B.
                self.restore_leaves_from_backing(w_xid, b_id);
            }
            // Stage 4c.4 round-3 finding: drop B's scene_participating
            // flag here so the protocol handler doesn't need a
            // separate `set_backing_scene_participation(false)`
            // call. No-op when the flag is already false (the
            // store's `set_scene_participating` short-circuits
            // the damage-clear branch when `was == v`).
            self.store.set_scene_participating(b_id, false);
        }
        if self.core.alias_registry.decref(backing) {
            self.free_pixmap(origin, raw)?;
        }
        // Step 2c — un-redirecting reverts each routed window to sampling its
        // own storage, which the scene diff sees as a signature change and
        // damages. But nothing here scheduled a tick, so that damage would sit
        // until some unrelated event woke the compositor. A wake carries no
        // region: if the diff finds nothing changed, the tick EmptyDamage-skips.
        //
        // Previously this was masked — the callers that reach here are usually
        // destroying the window too, and `destroy_subwindow` wakes. "Usually
        // masked" is not a guarantee.
        self.scene.wake_for_damage();
        Ok(())
    }

    // ── Resources (pixmap / font / cursor) ──────────────────────
    pub(in crate::kms::render::backend) fn backend_redirect_create_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        let xid = self.core.next_host_xid();
        // Stage 2c: allocate real backing storage. The engine
        // needs a live VkContext to paint into; on the test
        // fixture the platform's `allocate_drawable_storage`
        // returns `ERROR_INITIALIZATION_FAILED` and we fall back
        // to logging a gap + returning the bare xid (tests that
        // don't paint still get a stable handle).
        match self
            .platform
            .allocate_drawable_storage(width, height, depth)
        {
            Ok(storage) => {
                if let Err(e) = self.store_alloc(xid, DrawableKind::Pixmap, depth, false, storage) {
                    log::warn!(
                        "render create_pixmap: store.allocate failed for xid {xid:#x}: {e:?}",
                    );
                } else {
                    self.telemetry.record_storage_allocation();
                    self.telemetry.record_image_view_create();
                    // Stage 3f.14 follow-on: clear the fresh pixmap
                    // storage to a known-zero value. X11 says new
                    // pixmaps are undefined content, but Vk
                    // DEVICE_LOCAL memory is *fully* undefined —
                    // random GPU-recycled bytes. Real X servers tend
                    // to get away with this because system allocators
                    // zero pages, but our Vk allocator doesn't.
                    //
                    // Concrete repro (mate + marco + xeyes resize):
                    // xeyes creates a fresh depth-24 pixmap, sets a
                    // SHAPE clip matching its eye outlines, draws
                    // the eyes (only the shape-clipped area gets
                    // paint), then Present-Pixmaps the whole pixmap
                    // to the window. The non-eye area of the pixmap
                    // still holds undefined Vk bytes; Present copies
                    // it verbatim → visible garbage in the window.
                    //
                    // Cleared values: depth-32 transparent black
                    // (0,0,0,0) — premul no-op for compositing;
                    // depth-1 / depth-8 / depth-24 opaque black
                    // (0,0,0,1) — matches "uninitialised pixel = 0"
                    // which clients typically assume.
                    if let Some(id) = self.store.lookup(xid) {
                        let color = default_window_init_color(depth);
                        let rect = ash::vk::Rect2D {
                            offset: ash::vk::Offset2D::default(),
                            extent: ash::vk::Extent2D {
                                width: u32::from(width.max(1)),
                                height: u32::from(height.max(1)),
                            },
                        };
                        if let Err(e) = self.engine.fill_rect(
                            &mut self.store,
                            &mut self.platform,
                            Dst::server_internal(id),
                            rect,
                            color,
                        ) {
                            log::debug!(
                                "render create_pixmap: initial fill failed for xid {xid:#x}: {e:?}"
                            );
                        }
                    }
                }
            }
            Err(vk_err)
                if vk_err == ash::vk::Result::ERROR_INITIALIZATION_FAILED
                    && self.platform.vk.is_none() =>
            {
                // Test fixture path — no Vk available.
                self.log_render_gap("create_pixmap_no_vk");
            }
            Err(vk_err) => {
                return Err(io::Error::other(format!(
                    "create_pixmap: allocate_drawable_storage {width}x{height} d{depth}: \
                     {vk_err:?}"
                )));
            }
        }
        PixmapHandle::from_raw(xid).ok_or_else(|| io::Error::other("create_pixmap: xid was 0"))
    }

    pub(in crate::kms::render::backend) fn backend_redirect_free_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        // If the current clip cache still has only metadata, freeze the CPU
        // bytes now while the source drawable is still live. This preserves
        // retain-after-free for CPU-clipped fills without paying the readback
        // on the masked-copy fast path.
        self.materialize_pending_clip_mask_cache_on_free(host_xid);
        // Do NOT evict clip_mask_cache on free. X11 retain-after-free semantics:
        // XSetClipMask snapshots the bitmap into the GC; freeing the source pixmap
        // MUST NOT change clipping. The frozen snapshot stays valid after free
        // (clip_cache_reusable returns true when store.lookup(xid) is None).
        // XID reuse is handled by DrawableId mismatch in clip_cache_reusable,
        // so no explicit eviction is needed for correctness.
        // Stage 4b: alias-registry-aware free path. When `host_xid`
        // names a COMPOSITE-redirect backing (via NameWindowPixmap
        // alias or the Reason-1 redirect hold), decref the registry
        // first; only drop the storage when refcount hits zero.
        // Otherwise (an ordinary pixmap) fall through to the
        // straight `store.decref` path.
        //
        // v1's `free_pixmap` (`kms/backend.rs:9637-9650`) does NOT
        // consult the registry — it gets away with this because
        // compositors typically call FreePixmap(alias) after
        // UnredirectWindow, so the registry has already been
        // torn down by `release_redirected_backing`. The protocol
        // doesn't guarantee that ordering though, and v2 gates
        // here so an early FreePixmap on a still-held alias
        // doesn't drop the backing while a redirect still uses it.
        // GLX-TFP (Task 2.4): resolve the DrawableId BEFORE any decref so
        // we can tear down (or defer) the export tracking after the normal
        // client decref runs. For an export-only entry (glx_refs == 0)
        // this releases our lifetime ref and frees the backing now; for
        // the defer case (glx_refs > 0) it is a no-op and our lifetime ref
        // keeps the storage alive until glXDestroyPixmap.
        let export_id = self.store.lookup(host_xid);

        if let Some(handle) = yserver_core::backend::PixmapHandle::from_raw(host_xid)
            && self.core.alias_registry.get(handle).is_some()
        {
            if self.core.alias_registry.decref(handle)
                && let Some(id) = self.store.lookup(host_xid)
            {
                self.store_decref_with_invalidate(id);
            }
            if let Some(id) = export_id {
                self.maybe_teardown_export(id);
            }
            return Ok(());
        }
        if let Some(id) = self.store.lookup(host_xid) {
            self.store_decref_with_invalidate(id);
        }
        if let Some(id) = export_id {
            self.maybe_teardown_export(id);
        }
        Ok(())
    }
}
