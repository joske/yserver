use super::*;

impl KmsBackend {
    /// Decode the wire-packed clip rectangle list (`Vec<u8>` of
    /// i16 x, i16 y, u16 w, u16 h tuples) into `Rectangle16`s in
    /// dst-coords (with the GC clip-origin already added). Returns
    /// `None` when the current GC clip is `None`. `Pixmap`-clip is
    /// returned as `None` for now — Stage 3f.3 promotes the
    /// pixmap-mask path; until then the clip is passed through
    /// (matches v1's pre-promotion behaviour).
    fn current_clip_rects_in_dst_space(&self) -> Option<Vec<Rectangle16>> {
        let ClipState::Rectangles { origin, rects } = &self.core.current_clip else {
            return None;
        };
        let bytes = &rects.rectangles;
        let mut out = Vec::with_capacity(bytes.len() / 8);
        for chunk in bytes.chunks_exact(8) {
            let cx = i32::from(i16::from_le_bytes([chunk[0], chunk[1]])) + i32::from(origin.0);
            let cy = i32::from(i16::from_le_bytes([chunk[2], chunk[3]])) + i32::from(origin.1);
            let cw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            let ch = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
            if cw <= 0 || ch <= 0 {
                continue;
            }
            out.push(Rectangle16 {
                x: cx.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                y: cy.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                width: cw.min(i32::from(u16::MAX)) as u16,
                height: ch.min(i32::from(u16::MAX)) as u16,
            });
        }
        Some(out)
    }

    /// Intersect each rect in `rects` against the current GC clip.
    /// Handles three states:
    ///   - `ClipState::None` → pass through (input unchanged).
    ///   - `ClipState::Rectangles` → rect-vs-rect intersection (mirrors v1).
    ///   - `ClipState::Pixmap` → per-pixel mask gating via
    ///     [`super::super::backend::rasterize_pixmap_mask_to_rects`]
    ///     against the cached CPU bytes for the current pixmap clip.
    pub(crate) fn intersect_with_current_clip(&self, rects: &[Rectangle16]) -> Vec<Rectangle16> {
        match &self.core.current_clip {
            ClipState::None => rects.to_vec(),
            ClipState::Rectangles { .. } => {
                let clip_rects = self.current_clip_rects_in_dst_space().unwrap_or_default();
                let mut out = Vec::with_capacity(rects.len());
                for r in rects {
                    let rx0 = i32::from(r.x);
                    let ry0 = i32::from(r.y);
                    let rx1 = rx0 + i32::from(r.width);
                    let ry1 = ry0 + i32::from(r.height);
                    for c in &clip_rects {
                        let cx0 = i32::from(c.x);
                        let cy0 = i32::from(c.y);
                        let cx1 = cx0 + i32::from(c.width);
                        let cy1 = cy0 + i32::from(c.height);
                        let ix0 = rx0.max(cx0);
                        let iy0 = ry0.max(cy0);
                        let ix1 = rx1.min(cx1);
                        let iy1 = ry1.min(cy1);
                        if ix0 < ix1 && iy0 < iy1 {
                            out.push(Rectangle16 {
                                x: ix0 as i16,
                                y: iy0 as i16,
                                width: (ix1 - ix0) as u16,
                                height: (iy1 - iy0) as u16,
                            });
                        }
                    }
                }
                out
            }
            ClipState::Pixmap { .. } => {
                // Missing cache = install/readback failed; degrade to no-paint
                // (safer than pass-through, which would obliterate prior
                // decoration).
                let Some(cache) = self.clip_mask_cache.as_ref() else {
                    return Vec::new();
                };
                crate::kms::backend::rasterize_pixmap_mask_to_rects(
                    rects,
                    &cache.bytes,
                    cache.width,
                    cache.height,
                    u32::from(cache.depth),
                    cache.row_stride,
                    cache.origin,
                )
            }
        }
    }

    /// Refresh a pixmap clip-mask from the live source pixmap when
    /// possible, then intersect `rects` against the current clip state.
    /// If the source pixmap has been freed after installation into the GC,
    /// the cached bytes remain valid and only the clip origin is updated.
    pub(in crate::kms::render::backend) fn intersect_with_current_clip_live(
        &mut self,
        rects: &[Rectangle16],
    ) -> Vec<Rectangle16> {
        let pixmap_clip = match &self.core.current_clip {
            ClipState::Pixmap { origin, pixmap } => Some((pixmap.as_raw(), *origin)),
            _ => None,
        };
        if let Some((xid, origin)) = pixmap_clip {
            // Reuse the frozen snapshot when valid: same xid + (source freed OR
            // same DrawableId + unchanged content_version). Re-reading the mask
            // via engine.get_image (a GPU readback) on EVERY clipped paint op
            // pinned the single-threaded loop under gkrellm, which paints
            // ~500x/s through a static depth-1 clip mask
            // (project_client_scheduling_fairness).
            let cached_xid = self.clip_mask_cache.as_ref().map(|c| c.pixmap_xid);
            // Round-2 disambiguation: hit / miss-other-xid (rotation thrash a
            // multi-entry cache would absorb) / miss-no-entry (cache absent/empty).
            self.telemetry.record_clip_cache_outcome(match cached_xid {
                Some(c) if c == xid => Some(true),
                Some(_) => Some(false),
                None => None,
            });
            self.install_clip_mask_cache(xid, origin);
            self.ensure_clip_mask_cache_cpu_bytes(xid, origin);
        }
        self.intersect_with_current_clip(rects)
    }

    fn clip_mask_row_stride(width: u16, depth: u8) -> Option<u32> {
        match depth {
            1 => Some(u32::from(width).div_ceil(32) * 4),
            8 => Some(u32::from(width).div_ceil(4) * 4),
            _ => None,
        }
    }

    fn pending_clip_mask_cache(
        &self,
        host_pixmap_xid: u32,
        origin: (i16, i16),
    ) -> Option<crate::kms::backend::ClipMaskCache> {
        let id = self.store.lookup(host_pixmap_xid)?;
        let (width, height, depth, content_version) = {
            let d = self.store.get(id)?;
            let extent = d.storage.extent;
            (
                u16::try_from(extent.width).ok()?,
                u16::try_from(extent.height).ok()?,
                d.depth,
                d.content_version,
            )
        };
        let row_stride = Self::clip_mask_row_stride(width, depth)?;
        Some(crate::kms::backend::ClipMaskCache {
            pixmap_xid: host_pixmap_xid,
            drawable_id: id,
            content_version,
            origin,
            width,
            height,
            depth,
            row_stride,
            cpu_bytes_pending: true,
            bytes: Vec::new(),
        })
    }

    pub(in crate::kms::render::backend) fn read_fill_pattern_cache(
        &mut self,
        host_pixmap_xid: u32,
        origin: (i16, i16),
    ) -> Option<FillPatternCache> {
        let id = self.store.lookup(host_pixmap_xid)?;
        let (depth, extent) = {
            let d = self.store.get(id)?;
            (d.depth, d.storage.extent)
        };
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::FillPattern);
        let bytes = self
            .engine
            .get_image(
                &mut self.store,
                &mut self.platform,
                Src::server_internal(id),
                rect,
                depth,
            )
            .ok()?;
        Some(FillPatternCache {
            pixmap_xid: host_pixmap_xid,
            origin,
            depth,
            width: extent.width,
            height: extent.height,
            bytes,
        })
    }

    /// Synchronously read a pixmap's full extent via `engine.get_image`
    /// and return a `ClipMaskCache` ready for `intersect_with_current_clip`
    /// consumption. Returns `None` if the pixmap isn't in the store, has
    /// an unsupported depth (anything other than 1/8), or the readback
    /// errors. Bytes are in X11 wire format per
    /// `kms::render::engine::pack_from_storage` — depth-1 packed LSB-first,
    /// scanline-padded to 32 bits; depth-8 one byte per pixel,
    /// scanline-padded to 32 bits.
    pub(crate) fn read_clip_mask_bytes(
        &mut self,
        host_pixmap_xid: u32,
        origin: (i16, i16),
    ) -> Option<crate::kms::backend::ClipMaskCache> {
        let mut cache = self.pending_clip_mask_cache(host_pixmap_xid, origin)?;
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D {
                width: u32::from(cache.width),
                height: u32::from(cache.height),
            },
        };
        // SyncBoundary-storm attribution (gkrellm): clip-mask read-back per
        // clipped paint op — 2 SyncBoundary flushes via engine.get_image.
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::ClipMask);
        let bytes = self
            .engine
            .get_image(
                &mut self.store,
                &mut self.platform,
                Src::server_internal(cache.drawable_id),
                rect,
                cache.depth,
            )
            .ok()?;
        cache.cpu_bytes_pending = false;
        cache.bytes = bytes;
        Some(cache)
    }

    /// True iff the cached clip mask may be reused for installed pixmap `xid`
    /// without a fresh readback. Frozen-snapshot policy:
    /// - source freed (`lookup(xid) == None`)  -> reuse (X11 retain-after-free)
    /// - source live -> reuse iff same DrawableId AND unchanged content_version.
    pub(in crate::kms::render::backend) fn clip_cache_reusable(&self, xid: u32) -> bool {
        let Some(cache) = self.clip_mask_cache.as_ref() else {
            return false;
        };
        if cache.pixmap_xid != xid {
            return false;
        }
        match self.store.lookup(xid) {
            None => true,
            Some(did) => {
                // store.get(did) is effectively infallible here: lookup
                // returning Some(did) means the entry is live in the store;
                // the is_some_and is purely defensive against internal
                // inconsistency.
                did == cache.drawable_id
                    && self
                        .store
                        .get(did)
                        .is_some_and(|d| d.content_version == cache.content_version)
            }
        }
    }

    /// Install (or reuse) the clip-mask cache for `xid` at `origin`.
    /// If the current cache is already valid for `xid` (same DrawableId +
    /// unchanged content_version, or source freed), only the origin field is
    /// updated — no CPU readback occurs. Otherwise only the metadata is
    /// refreshed; the run-based clip path materializes bytes lazily while the
    /// masked-copy GPU snapshot still refreshes eagerly.
    pub(in crate::kms::render::backend) fn install_clip_mask_cache(
        &mut self,
        xid: u32,
        origin: (i16, i16),
    ) {
        if self.clip_cache_reusable(xid) {
            if let Some(c) = self.clip_mask_cache.as_mut() {
                c.origin = origin;
            }
        } else {
            self.clip_mask_cache = self.pending_clip_mask_cache(xid, origin);
        }
        // Task 14: GPU clip-mask snapshot lifecycle + EAGER POPULATION.
        // Hooked into the SHARED sink so all three install callers
        // (`set_clip_pixmap`, `apply_clip_state`, per-paint
        // `intersect_with_current_clip_live`) are covered. Idempotent:
        // create only on xid/drawable/size change; refresh no-ops when the
        // version already matches. Only acts when `xid` resolves to a LIVE
        // drawable; the snapshot is RETAINED across clip→None and pixmap
        // free (`install_clip_mask_cache` is not called for None), so a
        // freed source keeps the GPU mask captured here while CPU bytes are
        // frozen either by a prior CPU clip use or by `free_pixmap`.
        self.refresh_clip_mask_snapshot_for(xid);
    }

    fn ensure_clip_mask_cache_cpu_bytes(&mut self, xid: u32, origin: (i16, i16)) {
        let needs_materialize = self
            .clip_mask_cache
            .as_ref()
            .is_some_and(|c| c.pixmap_xid == xid && c.cpu_bytes_pending);
        if needs_materialize {
            self.clip_mask_cache = self.read_clip_mask_bytes(xid, origin);
        }
    }

    pub(in crate::kms::render::backend) fn materialize_pending_clip_mask_cache_on_free(
        &mut self,
        host_xid: u32,
    ) {
        let Some(cache) = self.clip_mask_cache.as_ref() else {
            return;
        };
        if cache.pixmap_xid != host_xid || !cache.cpu_bytes_pending {
            return;
        }
        self.clip_mask_cache = self.read_clip_mask_bytes(host_xid, cache.origin);
    }

    /// Create-or-reuse + eagerly populate the GPU clip-mask snapshot for the
    /// live mask pixmap `xid` (Task 14 Step 2). No-op if `xid` is not a live
    /// drawable (retain-after-free: the existing snapshot, if any, stays).
    /// A `refresh_clip_snapshot` error is logged + swallowed so a transient
    /// failure degrades to the run-based CPU path rather than failing the
    /// paint request.
    fn refresh_clip_mask_snapshot_for(&mut self, xid: u32) {
        // Only populate while the source pixmap is live.
        let Some(did) = self.store.lookup(xid) else {
            return;
        };
        let Some((w, h, live_version)) = self.store.get(did).and_then(|d| {
            let e = d.storage.extent;
            (e.width != 0 && e.height != 0).then_some((e.width, e.height, d.content_version))
        }) else {
            return;
        };
        // (Re)create the snapshot on first use or on xid/drawable/size change.
        let needs_new = match &self.clip_mask_snapshot {
            Some(s) => s.pixmap_xid != xid || s.drawable_id != did || s.width != w || s.height != h,
            None => true,
        };
        if needs_new {
            if let Some(old) = self.clip_mask_snapshot.take() {
                self.engine.retire_clip_snapshot(old.id);
            }
            match self.engine.create_clip_snapshot(w, h) {
                Ok(id) => {
                    self.clip_mask_snapshot = Some(ClipMaskSnapshot {
                        pixmap_xid: xid,
                        drawable_id: did,
                        id,
                        width: w,
                        height: h,
                    });
                }
                Err(e) => {
                    // Engine has no Vulkan inner (test/headless) or alloc
                    // failed — leave the snapshot absent; masked route falls
                    // through to the run-based path.
                    log::warn!(
                        "render clip-mask snapshot create failed (xid=0x{xid:x}, {w}x{h}); \
                         masked CopyArea will degrade to the run-based path: {e:?}",
                    );
                    return;
                }
            }
        }
        let sid = self.clip_mask_snapshot.as_ref().unwrap().id;
        // Eagerly populate WHILE THE PIXMAP IS LIVE (closes retain-after-free
        // before first use). No-ops when the version already matches.
        if let Err(e) = self.engine.refresh_clip_snapshot(
            &mut self.store,
            &mut self.platform,
            sid,
            did,
            live_version,
        ) {
            log::warn!(
                "render clip-mask snapshot refresh failed (xid=0x{xid:x}); \
                 masked CopyArea will degrade to the run-based path: {e:?}",
            );
        }
    }

    /// Storage dimensions for a host xid, in pixels. `None` if the
    /// drawable is unknown.
    pub(in crate::kms::render::backend) fn drawable_dims(
        &self,
        host_xid: u32,
    ) -> Option<(u32, u32)> {
        let id = self.store.lookup(host_xid)?;
        let d = self.store.get(id)?;
        Some((d.storage.extent.width, d.storage.extent.height))
    }

    /// Lower a list of solid-colour rectangles to the appropriate
    /// engine path. Used by the stroke-style poly ops (`PolyLine`,
    /// `PolySegment`, `PolyPoint`, `PolyArc`, `PolyRectangle`) where
    /// every rasterised rect is in the GC's single foreground colour
    /// regardless of GC fill-style, and as the fallback inside
    /// Stage 4a — shift each rect by `(dx, dy)` (saturating to
    /// i16 range). Returns the input unchanged when both deltas
    /// are zero. Used to translate window-local paint rects into
    /// backing-local coords under COMPOSITE redirect: a paint
    /// against descendant C of redirected W at offset
    /// `(cx, cy)` against W lands at C's rect + `(cx, cy)` in
    /// W's backing.
    pub(in crate::kms::render::backend) fn shift_rectangles_for_paint(
        rects: &[Rectangle16],
        (dx, dy): (i32, i32),
    ) -> std::borrow::Cow<'_, [Rectangle16]> {
        if dx == 0 && dy == 0 {
            return std::borrow::Cow::Borrowed(rects);
        }
        std::borrow::Cow::Owned(
            rects
                .iter()
                .map(|r| Rectangle16 {
                    x: (i32::from(r.x) + dx).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                    y: (i32::from(r.y) + dy).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                    width: r.width,
                    height: r.height,
                })
                .collect(),
        )
    }

    /// A backing-space destination clip (`None` = unclipped) narrowed to
    /// where `dst_host_xid` may draw in a backing it shares
    /// ([`Self::shared_backing_draw_clip`]).
    pub(in crate::kms::render::backend) fn narrow_dst_clip_to_shared_backing(
        &self,
        dst_host_xid: u32,
        target: &PaintTarget,
        dst_clip: Option<Vec<Rectangle16>>,
    ) -> Option<Vec<Rectangle16>> {
        let Some(keep) = self.shared_backing_draw_clip(dst_host_xid, target) else {
            return dst_clip;
        };
        let (dx, dy) = target.offset();
        let keep: Vec<ash::vk::Rect2D> = keep
            .into_iter()
            .map(|r| ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: r.offset.x + dx,
                    y: r.offset.y + dy,
                },
                extent: r.extent,
            })
            .collect();
        let unclipped = [Rectangle16 {
            x: i16::MIN,
            y: i16::MIN,
            width: u16::MAX,
            height: u16::MAX,
        }];
        Some(clip_rects16(
            dst_clip.as_deref().unwrap_or(&unclipped),
            &keep,
        ))
    }

    /// Stage 4a — shift a picture's clip rects from
    /// dst-drawable-local into backing-local coords. The clip
    /// itself is stored in dst-window-local coords (pre-shifted
    /// by Stage 3b's `clip_x` / `clip_y`); when paint resolves
    /// through a redirected ancestor, the per-rect scissor in
    /// the engine operates against the backing's storage extent,
    /// so the clip must move with it.
    pub(in crate::kms::render::backend) fn shift_dst_picture_clip(
        clip: Option<Vec<Rectangle16>>,
        offset: (i32, i32),
    ) -> Option<Vec<Rectangle16>> {
        let rects = clip?;
        Some(Self::shift_rectangles_for_paint(&rects, offset).into_owned())
    }

    /// Apply the GC's subwindow mode to fill-style rects expressed in the
    /// destination window's local coordinates. `ClipByChildren` subtracts
    /// every mapped automatic child window (by its bounding shape when it
    /// has one); `IncludeInferiors` leaves the rects unchanged. Either way
    /// a window drawing into a backing it shares with its ancestors stays
    /// where [`Self::shared_backing_draw_clip`] lets it.
    pub(in crate::kms::render::backend) fn clip_fill_rects_by_subwindow_mode(
        &self,
        host_xid: u32,
        rects: &[Rectangle16],
    ) -> Vec<Rectangle16> {
        if rects.is_empty() {
            return Vec::new();
        }
        match self.subwindow_mode_clip(host_xid, self.core.current_subwindow_mode) {
            Some(clip) => apply_subwindow_mode_clip(&clip, rects),
            None => rects.to_vec(),
        }
    }

    /// What [`Self::clip_fill_rects_by_subwindow_mode`] cuts `host_xid`'s
    /// rects with, computed once for a request whatever its rect count:
    /// the children it takes out, and where it may draw in a shared
    /// backing. `None` when nothing does.
    pub(in crate::kms::render::backend) fn subwindow_mode_clip(
        &self,
        host_xid: u32,
        mode: yserver_core::backend::SubwindowMode,
    ) -> Option<SubwindowModeClip> {
        let key = (
            host_xid,
            mode,
            self.windows.generation(),
            self.store.topology_generation(),
            self.shape_generation,
        );
        if let Some(entry) = self.subwindow_clip_cache.borrow().as_ref()
            && entry.key == key
        {
            return entry.clip.clone();
        }
        let clip = self.compute_subwindow_mode_clip(host_xid, mode);
        *self.subwindow_clip_cache.borrow_mut() = Some(SubwindowClipCacheEntry {
            key,
            clip: clip.clone(),
        });
        clip
    }

    fn compute_subwindow_mode_clip(
        &self,
        host_xid: u32,
        mode: yserver_core::backend::SubwindowMode,
    ) -> Option<SubwindowModeClip> {
        if !self.windows.contains_key(&host_xid) {
            return None;
        }
        let mut cut: Vec<ash::vk::Rect2D> = Vec::new();
        if matches!(mode, yserver_core::backend::SubwindowMode::ClipByChildren) {
            cut = self
                .windows
                .iter()
                .filter(|(child_host_xid, geom)| {
                    geom.parent == Some(host_xid)
                        && geom.mapped
                        && !self
                            .store
                            .lookup(**child_host_xid)
                            .and_then(|id| self.store.get(id))
                            .is_some_and(|d| !d.scene_participating)
                })
                .flat_map(|(child_host_xid, geom)| {
                    // #133 step 3 round 5 — the same content-space rule as
                    // the `IncludeInferiors` fan-out: a child occupies
                    // `(x + bw, y + bw, w, h)` of its parent's CONTENT
                    // space, because `x`/`y` are its OUTER origin
                    // (`dix/window.c`: `drawable.x = parent->drawable.x + x
                    // + bw`) and these rects are subtracted from a draw
                    // expressed in the parent's content coordinates.
                    // Identity at `bw == 0`.
                    //
                    // NOTE Xorg subtracts the child's BORDER-inclusive box
                    // here (`clipList` excludes a child's whole outer
                    // extent, `mi/mivaltree.c`); yserver subtracts the
                    // content box, which is what it has always done. That
                    // difference is pre-existing and belongs to step 5's
                    // scene/clip work, not to this fix.
                    let child_bw = i32::from(geom.border_width);
                    let content_box = ash::vk::Rect2D {
                        offset: ash::vk::Offset2D {
                            x: i32::from(geom.x) + child_bw,
                            y: i32::from(geom.y) + child_bw,
                        },
                        extent: ash::vk::Extent2D {
                            width: u32::from(geom.width),
                            height: u32::from(geom.height),
                        },
                    };
                    self.child_clip_region(*child_host_xid, geom, content_box)
                })
                .collect();
        }
        let keep = self
            .resolve_paint_target(host_xid)
            .and_then(|t| self.shared_backing_draw_clip(host_xid, &t));
        if cut.is_empty() && keep.is_none() {
            return None;
        }
        Some(SubwindowModeClip { cut, keep })
    }
}

/// `v + by`, saturated to the wire's `INT16`.
pub(in crate::kms::render::backend) fn shift_i16(v: i16, by: i32) -> i16 {
    (i32::from(v) + by).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// `rects` through `clip`.
pub(in crate::kms::render::backend) fn apply_subwindow_mode_clip(
    clip: &SubwindowModeClip,
    rects: &[Rectangle16],
) -> Vec<Rectangle16> {
    let (cut, keep) = (&clip.cut, &clip.keep);
    // Text and lines come as many small spans: when the clip leaves
    // their bounding box whole, take them as they are.
    if let Some(bbox) = rects16_bbox(rects) {
        let (bx1, by1) = (
            bbox.offset.x + bbox.extent.width as i32,
            bbox.offset.y + bbox.extent.height as i32,
        );
        let misses = |c: &ash::vk::Rect2D| {
            c.offset.x >= bx1
                || c.offset.y >= by1
                || c.offset.x + c.extent.width as i32 <= bbox.offset.x
                || c.offset.y + c.extent.height as i32 <= bbox.offset.y
        };
        let covers = |k: &ash::vk::Rect2D| {
            k.offset.x <= bbox.offset.x
                && k.offset.y <= bbox.offset.y
                && k.offset.x + k.extent.width as i32 >= bx1
                && k.offset.y + k.extent.height as i32 >= by1
        };
        if cut.iter().all(misses) && keep.as_ref().is_none_or(|k| k.iter().any(covers)) {
            return rects.to_vec();
        }
    }
    let mut out = Vec::with_capacity(rects.len());
    for r in rects {
        if r.width == 0 || r.height == 0 {
            continue;
        }
        // The same test per rect: most spans need no cutting.
        let (rx0, ry0) = (i32::from(r.x), i32::from(r.y));
        let (rx1, ry1) = (rx0 + i32::from(r.width), ry0 + i32::from(r.height));
        let clear = cut.iter().all(|c| {
            c.offset.x >= rx1
                || c.offset.y >= ry1
                || c.offset.x + c.extent.width as i32 <= rx0
                || c.offset.y + c.extent.height as i32 <= ry0
        }) && keep.as_ref().is_none_or(|k| {
            k.iter().any(|k| {
                k.offset.x <= rx0
                    && k.offset.y <= ry0
                    && k.offset.x + k.extent.width as i32 >= rx1
                    && k.offset.y + k.extent.height as i32 >= ry1
            })
        });
        if clear {
            out.push(*r);
            continue;
        }
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(r.x),
                y: i32::from(r.y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(r.width),
                height: u32::from(r.height),
            },
        };
        let pieces = match &keep {
            Some(keep) => intersect_rect_with_clip(rect, keep),
            None => vec![rect],
        };
        out.extend(
            pieces
                .into_iter()
                .flat_map(|piece| compute_copy_area_dst_rects(piece, cut))
                .filter_map(|piece| {
                    Some(Rectangle16 {
                        x: i16::try_from(piece.offset.x).ok()?,
                        y: i16::try_from(piece.offset.y).ok()?,
                        width: u16::try_from(piece.extent.width).ok()?,
                        height: u16::try_from(piece.extent.height).ok()?,
                    })
                }),
        );
    }
    out
}

/// Stage 4d Manual-redirect fix: when a window has mapped child
/// windows and is drawn into via a `ClipByChildren` GC (the X11
/// default), the draw must NOT touch the area covered by each
/// child. In v1 this was natural because every window had its own
/// mirror — paint to the parent landed in the parent's storage
/// while child paint landed in the child's. v2's COMPOSITE Manual-
/// redirect collapses an entire redirected subtree into a single
/// backing pixmap, so the parent-vs-child overlap is now a real
/// region-rect subtraction the backend has to perform.
///
/// Symptom this fixes: marco's per-frame full-extent CopyArea
/// (decorations source → frame) clobbers the inferior CC window's
/// area inside the redirected backing, then CC repaints only its
/// small dirty rect, leaving the backing's centre as marco's
/// (mostly-blank) decoration pixmap. Visible as "top-left content
/// only" after a few frames.
///
/// `dst_rect` is in destination-window-local coordinates; `child_rects`
/// are mapped-child rectangles also in destination-window-local
/// coordinates (parent's `(child.x, child.y, child.w, child.h)`).
/// Returns the surviving sub-rectangles, also in dst-window-local
/// coordinates. Empty input child list returns `[dst_rect]`. Empty
/// `dst_rect` (zero-size) returns `[]`.
/// Intersect a destination rectangle against an X11 GC clip
/// (a list of rectangles already translated into destination-window
/// coordinates). Returns the surviving pieces. An empty `clip_rects`
/// represents an empty clip region — Xorg's behaviour is "paint
/// nothing", so we return an empty Vec. `rect` with zero area also
/// returns empty.
///
/// Used by `copy_area` ahead of child subtraction so a
/// `SetClipRectangles`-issued explicit clip constrains the copy
/// (Stage 4d codex round 2026-05-18: pre-fix `copy_area` honoured
/// neither GC clip nor `ClipByChildren`).
pub(in crate::kms::render::backend) fn intersect_rect_with_clip(
    rect: ash::vk::Rect2D,
    clip_rects: &[ash::vk::Rect2D],
) -> Vec<ash::vk::Rect2D> {
    if clip_rects.is_empty() || rect.extent.width == 0 || rect.extent.height == 0 {
        return Vec::new();
    }
    let rx0 = rect.offset.x;
    let ry0 = rect.offset.y;
    let rx1 = rx0 + i32::try_from(rect.extent.width).unwrap_or(i32::MAX);
    let ry1 = ry0 + i32::try_from(rect.extent.height).unwrap_or(i32::MAX);
    let mut out = Vec::with_capacity(clip_rects.len());
    for c in clip_rects {
        let cx0 = c.offset.x;
        let cy0 = c.offset.y;
        let cx1 = cx0 + i32::try_from(c.extent.width).unwrap_or(0);
        let cy1 = cy0 + i32::try_from(c.extent.height).unwrap_or(0);
        let ix0 = rx0.max(cx0);
        let iy0 = ry0.max(cy0);
        let ix1 = rx1.min(cx1);
        let iy1 = ry1.min(cy1);
        if ix0 < ix1 && iy0 < iy1 {
            out.push(ash::vk::Rect2D {
                offset: ash::vk::Offset2D { x: ix0, y: iy0 },
                extent: ash::vk::Extent2D {
                    width: u32::try_from(ix1 - ix0).unwrap_or(0),
                    height: u32::try_from(iy1 - iy0).unwrap_or(0),
                },
            });
        }
    }
    out
}

/// Translate a clip-rect list by `(dx, dy)` (signed). Used to map a
/// source / mask picture's client clip from the picture's own
/// drawable space into the destination's drawable space, mirroring
/// Xorg's `miClipPictureSrc`
/// (`/home/jos/Projects/xserver/render/mipict.c:267-290`). The Xorg
/// path translates pPicture->clientClip in-place, intersects, then
/// translates back; we copy-translate so the picture record stays
/// untouched.
///
/// Out-of-i16 results saturate; X11 fixed-point clips never need
/// more than 16-bit signed coords on the wire.
pub(in crate::kms::render::backend) fn translate_clip_rects(
    rects: &[Rectangle16],
    dx: i32,
    dy: i32,
) -> Vec<Rectangle16> {
    rects
        .iter()
        .map(|r| {
            let nx = i32::from(r.x).saturating_add(dx);
            let ny = i32::from(r.y).saturating_add(dy);
            Rectangle16 {
                x: i16::try_from(nx).unwrap_or(if nx < 0 { i16::MIN } else { i16::MAX }),
                y: i16::try_from(ny).unwrap_or(if ny < 0 { i16::MIN } else { i16::MAX }),
                width: r.width,
                height: r.height,
            }
        })
        .collect()
}

/// Intersect two clip-rect lists. Returns the pairwise rectangle
/// intersections, omitting empties. Both lists are interpreted as
/// "the clip is the union of these rects" — the resulting list is
/// `union { a ∩ b : a ∈ a_list, b ∈ b_list }`.
///
/// Helper for `compute_render_composite_clip` below.
pub(in crate::kms::render::backend) fn intersect_clip_lists(
    a: &[Rectangle16],
    b: &[Rectangle16],
) -> Vec<Rectangle16> {
    let mut out = Vec::with_capacity(a.len() * b.len());
    for ra in a {
        let ax0 = i32::from(ra.x);
        let ay0 = i32::from(ra.y);
        let ax1 = ax0.saturating_add(i32::from(ra.width));
        let ay1 = ay0.saturating_add(i32::from(ra.height));
        for rb in b {
            let bx0 = i32::from(rb.x);
            let by0 = i32::from(rb.y);
            let bx1 = bx0.saturating_add(i32::from(rb.width));
            let by1 = by0.saturating_add(i32::from(rb.height));
            let ix0 = ax0.max(bx0);
            let iy0 = ay0.max(by0);
            let ix1 = ax1.min(bx1);
            let iy1 = ay1.min(by1);
            if ix0 < ix1 && iy0 < iy1 {
                out.push(Rectangle16 {
                    x: i16::try_from(ix0).unwrap_or(i16::MAX),
                    y: i16::try_from(iy0).unwrap_or(i16::MAX),
                    width: u16::try_from(ix1 - ix0).unwrap_or(u16::MAX),
                    height: u16::try_from(iy1 - iy0).unwrap_or(u16::MAX),
                });
            }
        }
    }
    out
}

pub(in crate::kms::render::backend) fn local_rects_to_region(
    rects: Vec<Rectangle16>,
) -> Vec<xfixes::RegionRect> {
    rects
        .into_iter()
        .map(|r| xfixes::RegionRect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        })
        .collect()
}

/// `rects` cut to `keep`, both in one coordinate space; pieces that no
/// longer fit the wire types are dropped.
pub(in crate::kms::render::backend) fn clip_rects16(
    rects: &[Rectangle16],
    keep: &[ash::vk::Rect2D],
) -> Vec<Rectangle16> {
    rects
        .iter()
        .filter(|r| r.width > 0 && r.height > 0)
        .flat_map(|r| {
            intersect_rect_with_clip(
                ash::vk::Rect2D {
                    offset: ash::vk::Offset2D {
                        x: i32::from(r.x),
                        y: i32::from(r.y),
                    },
                    extent: ash::vk::Extent2D {
                        width: u32::from(r.width),
                        height: u32::from(r.height),
                    },
                },
                keep,
            )
        })
        .filter_map(|r| {
            Some(Rectangle16 {
                x: i16::try_from(r.offset.x).ok()?,
                y: i16::try_from(r.offset.y).ok()?,
                width: u16::try_from(r.extent.width).ok()?,
                height: u16::try_from(r.extent.height).ok()?,
            })
        })
        .collect()
}

/// Subtract `inner` from `outer`. Both rects are in the same coord
/// space. Result is up to 4 disjoint sub-rectangles tiling
/// `outer \ inner` (top strip, bottom strip, middle-band left strip,
/// middle-band right strip — Xorg/pixman band order). If `inner`
/// doesn't intersect `outer`, returns `[outer]` unchanged.
pub(in crate::kms::render::backend) fn subtract_one_rect_clip(
    outer: ash::vk::Rect2D,
    inner: ash::vk::Rect2D,
) -> Vec<ash::vk::Rect2D> {
    let ox0 = outer.offset.x;
    let oy0 = outer.offset.y;
    let ox1 = outer.offset.x + i32::try_from(outer.extent.width).unwrap_or(i32::MAX);
    let oy1 = outer.offset.y + i32::try_from(outer.extent.height).unwrap_or(i32::MAX);
    // Intersection of inner with outer (clamped to outer's bounds).
    let ix0 = inner.offset.x.max(ox0);
    let iy0 = inner.offset.y.max(oy0);
    let ix1 = (inner.offset.x + i32::try_from(inner.extent.width).unwrap_or(0)).min(ox1);
    let iy1 = (inner.offset.y + i32::try_from(inner.extent.height).unwrap_or(0)).min(oy1);
    if ix0 >= ix1 || iy0 >= iy1 {
        return vec![outer];
    }
    let mk = |x: i32, y: i32, w: i32, h: i32| ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x, y },
        extent: ash::vk::Extent2D {
            width: u32::try_from(w).unwrap_or(0),
            height: u32::try_from(h).unwrap_or(0),
        },
    };
    let mut result = Vec::with_capacity(4);
    // Top strip: full outer width, y in [oy0, iy0).
    if oy0 < iy0 {
        result.push(mk(ox0, oy0, ox1 - ox0, iy0 - oy0));
    }
    // Bottom strip: full outer width, y in [iy1, oy1).
    if iy1 < oy1 {
        result.push(mk(ox0, iy1, ox1 - ox0, oy1 - iy1));
    }
    // Left middle: middle band height, x in [ox0, ix0).
    if ox0 < ix0 {
        result.push(mk(ox0, iy0, ix0 - ox0, iy1 - iy0));
    }
    // Right middle: middle band height, x in [ix1, ox1).
    if ix1 < ox1 {
        result.push(mk(ix1, iy0, ox1 - ix1, iy1 - iy0));
    }
    result
}
