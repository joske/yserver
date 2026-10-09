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
