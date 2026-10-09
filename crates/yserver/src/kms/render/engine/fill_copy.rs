use super::*;

impl RenderEngine {
    // ── Op: fill_rect / fill_rect_batch ─────────────────────────

    /// Fill `rect` in `target`'s storage with `color` (RGBA float).
    /// Convenience wrapper around [`Self::fill_rect_batch`] for the
    /// single-rect call sites (create_pixmap zero-fill, bg_pixel
    /// init, image_text background, etc.).
    ///
    /// # Errors
    ///
    /// - `NoVk`, `UnknownDrawable`, `RendererFailed`, or any
    ///   propagated `vk::Result` from CB allocation / submit.
    pub(crate) fn fill_rect(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        target: Dst,
        rect: vk::Rect2D,
        color: [f32; 4],
    ) -> Result<(), RenderError> {
        // Phase B.3 (N4): one-line delegate — fill_rect_batch carries the
        // new frame-builder body. The old close_open_frame_for_non_ported_op
        // call is DELETED; fill_rect now extends the open frame instead of
        // closing it.
        self.fill_rect_batch(store, platform, target, color, &[rect])
    }

    /// Fill every rect in `rects` on `target` with `color`, accumulating
    /// ONE `RecordedFillRect` into the open frame (Phase B.3 N4 — the
    /// entire rect slice is ONE op, not split per-rect). `fill_rect` is
    /// a one-line delegate here with N=1.
    ///
    /// Body order per N9: empty-input fast-path → `renderer_failed` →
    /// `flush_render_batch` → preflight (clamp+filter) → open frame if
    /// not open → first_touch + ticket-touch + damage →
    /// `push_op_and_set_layouts` with `(target, SHADER_READ_ONLY_OPTIMAL)`.
    ///
    /// Zero-sized rects are filtered up-front; if the slice contains
    /// only empties (or is empty), the call short-circuits without
    /// touching the frame.
    ///
    /// # Errors
    ///
    /// - `NoVk`, `UnknownDrawable`, `RendererFailed`, or any
    ///   propagated `vk::Result` from CB allocation (on frame open).
    pub(crate) fn fill_rect_batch(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst: Dst,
        color: [f32; 4],
        rects: &[vk::Rect2D],
    ) -> Result<(), RenderError> {
        let target = dst.id();
        // Phase B.3 (N9): empty-input fast-path — BEFORE flush_render_batch.
        if rects.is_empty() {
            return Ok(());
        }
        // Phase B.3 (N9): renderer_failed check before any open-frame mutation.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // Phase B.3 (N9): flush pending_render_batch at entry. May close an
        // open frame (chronological X11 ordering with pre-existing batches).
        // NO flush_cow_batch — that helper is deleted in Task 4.
        self.flush_render_batch(store, platform, RenderFlushReason::Fill)?;

        // Preflight: read target metadata WITHOUT mutating the frame.
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let Some(drawable) = store.get(target) else {
            return Err(RenderError::UnknownDrawable(target));
        };
        let extent = drawable.storage.extent;
        let image_view = drawable.storage.image_view;
        let format = drawable.storage.format;
        let dst_pre_layout = inner.current_layout_for_drawable(store, target);
        let prior_dst_ticket = drawable.last_render_ticket.clone();

        // Clamp + drop empties up front. Doing this before any frame
        // mutation means an all-empty batch short-circuits cleanly.
        // #133 step 3 (P4): clamp to the destination handle's bounds —
        // the content rect for a bordered window, the whole storage
        // otherwise. Fills can no longer reach the border ring.
        let clamped: Vec<vk::Rect2D> = rects
            .iter()
            .map(|r| clamp_rect_to(*r, dst.bounds_in(extent)))
            .filter(|r| r.extent.width != 0 && r.extent.height != 0)
            .collect();
        if clamped.is_empty() {
            return Ok(());
        }

        // Open the frame if not already open (mirror copy_area / put_image
        // pattern at engine.rs:5278-5287).
        if !inner.frame_builder.is_open() {
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();

        // Prelude: first_touch + ticket-touch + damage.
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(target, prior_dst_ticket);
            open.layouts.first_touch_drawable(target, dst_pre_layout);
        }
        store.touch_render_fence(target, frame_ticket.clone());
        for r in &clamped {
            store.damage(target, *r);
        }

        // Phase B.3 (N4): ONE RecordedFillRect per call carrying the entire
        // clamped rect slice. Splitting per-rect would be new behavior.
        let payload = Box::new(crate::kms::render::frame_builder::RecordedFillRect {
            dst_id: target,
            dst_image_view: image_view,
            dst_extent: extent,
            dst_format: format,
            dst_old_layout: dst_pre_layout,
            color,
            rects: clamped,
        });
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                crate::kms::render::frame_builder::RecordedOp::FillRect(payload),
                &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }
        store.mark_contents_modified(target);
        Ok(())
    }

    // ── Op: logic_fill (Stage 3f.2) ─────────────────────────────

    /// Lazy-build a `LogicFillPipelineCache` for `color_format`.
    /// Each cache instance is bound to a single attachment format
    /// at construction; we shard by format so a session that paints
    /// to both BGRA8 and R8 dst formats only pays per-format pipeline
    /// compile cost. The inner cache further keys by `(function,
    /// opaque_alpha)` so all 16 X11 GC functions × {opaque, ARGB}
    /// share one pipeline-layout.
    fn ensure_logic_fill_cache(
        &mut self,
        platform: &PlatformBackend,
        color_format: vk::Format,
    ) -> Result<(), RenderError> {
        use crate::kms::vk::logic_fill_pipeline::LogicFillPipelineCache;
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        if inner.logic_fill_caches.contains_key(&color_format) {
            return Ok(());
        }
        let cache =
            LogicFillPipelineCache::new(Arc::clone(&inner.vk), color_format).map_err(|e| {
                log::error!(
                    "render ensure_logic_fill_cache: LogicFillPipelineCache::new failed: {e:?}"
                );
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        inner.logic_fill_caches.insert(color_format, cache);
        Ok(())
    }

    /// Solid-fill `rects` in `target` through a `VkLogicOp` pipeline
    /// matching `function`. Ports v1's `try_vk_fill_with_function`
    /// non-`GXcopy` path into a v2-shape per-op CB.
    ///
    /// `opaque_alpha = true` is the depth-24 (server-owned α) path:
    /// the pipeline's color blend write mask drops alpha so the
    /// `VkLogicOp` only mutates RGB and the destination's existing
    /// alpha byte is left intact (L1 server-α invariant). `false` is
    /// the depth-32 ARGB path — LogicOp applies to all four channels
    /// per X11 semantics.
    ///
    /// `fg` is the X11 wire pixel value (top byte alpha for depth 32,
    /// ignored for depth 24). The recorder unpacks it identically to
    /// v1's `try_vk_fill_with_function`. `GXclear`-class functions
    /// (Clear / Set / Invert / etc.) ignore `fg` semantically; the
    /// fragment shader still receives it but `VkLogicOp` overrides
    /// the output.
    ///
    /// # Errors
    ///
    /// `UnknownDrawable` if `target` is missing; `NoVk` on the stub
    /// engine; `Vk` for any underlying Vulkan failure.
    pub(crate) fn logic_fill(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst: Dst,
        function: yserver_core::backend::GcFunction,
        opaque_alpha: bool,
        fg: u32,
        rects: &[Rectangle16],
    ) -> Result<(), RenderError> {
        self.logic_fill_channels(
            store,
            platform,
            dst,
            function,
            crate::kms::vk::logic_fill_pipeline::LogicFillChannels::from_opaque_alpha(opaque_alpha),
            fg,
            rects,
        )
    }

    /// Set alpha to 0xFF over `rects` (storage coordinates, clamped to
    /// `dst`'s bounds) and leave RGB untouched.
    ///
    /// For a depth-24 window that paints into a depth-32 ancestor's
    /// redirect backing: X gives such a window no alpha, so its pixels
    /// must read opaque there — Xorg composites a mismatched-depth child
    /// into its parent with alpha forced to 1. A raw image copy carries
    /// the source's undefined X byte across instead (a GL client's
    /// background is commonly 0), so the copy is followed by this stamp.
    ///
    /// # Errors
    ///
    /// As [`Self::logic_fill`].
    pub(crate) fn stamp_opaque_alpha(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst: Dst,
        rects: &[Rectangle16],
    ) -> Result<(), RenderError> {
        self.logic_fill_channels(
            store,
            platform,
            dst,
            yserver_core::backend::GcFunction::Set,
            crate::kms::vk::logic_fill_pipeline::LogicFillChannels::Alpha,
            0,
            rects,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn logic_fill_channels(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst: Dst,
        function: yserver_core::backend::GcFunction,
        channels: crate::kms::vk::logic_fill_pipeline::LogicFillChannels,
        fg: u32,
        rects: &[Rectangle16],
    ) -> Result<(), RenderError> {
        use yserver_core::backend::GcFunction;

        let target = dst.id();
        // N9 order: empty-input fast-paths → renderer_failed →
        // flush_render_batch → preflight → cache ensure → open →
        // prelude → push.
        if rects.is_empty() {
            return Ok(());
        }
        if matches!(function, GcFunction::NoOp) {
            return Ok(());
        }
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        self.flush_render_batch(store, platform, RenderFlushReason::Fill)?;

        // Preflight: read format via shared borrow; build pipeline cache
        // for the dst format if not already present (preserve the
        // `ensure_logic_fill_cache` helper verbatim per N6).
        let format = {
            let d = store
                .get(target)
                .ok_or(RenderError::UnknownDrawable(target))?;
            d.storage.format
        };
        self.ensure_logic_fill_cache(platform, format)?;

        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let Some(drawable) = store.get(target) else {
            return Err(RenderError::UnknownDrawable(target));
        };
        let extent = drawable.storage.extent;
        let depth = drawable.depth;
        let image_view = drawable.storage.image_view;
        let dst_pre_layout = inner.current_layout_for_drawable(store, target);
        let prior_dst_ticket = drawable.last_render_ticket.clone();

        // Unpack the X11 wire pixel (preserve legacy at engine.rs:2546-2560).
        // R8_UNORM dst (depth 1/8) takes fg in color[0]; BGRA8 dst uses the
        // same server-alpha policy as solid fills: depth-32 preserves the wire
        // alpha byte, server-owned-alpha depths force opaque.
        let color = decode_x11_pixel_for_storage(fg, depth, format);

        // Clamp rects to the dst BOUNDS + drop empties (preserve legacy
        // filter_map at engine.rs:2562-2588; #133 step 3 (P4) swapped the
        // storage extent for the handle's content bounds).
        let bounds = dst.bounds_in(extent);
        let bounds_x1 = bounds.offset.x.saturating_add_unsigned(bounds.extent.width);
        let bounds_y1 = bounds
            .offset
            .y
            .saturating_add_unsigned(bounds.extent.height);
        let vk_rects: Vec<vk::Rect2D> = rects
            .iter()
            .filter_map(|r| {
                let x0 = i32::from(r.x).max(bounds.offset.x);
                let y0 = i32::from(r.y).max(bounds.offset.y);
                let x1 = (i32::from(r.x).saturating_add(i32::from(r.width))).min(bounds_x1);
                let y1 = (i32::from(r.y).saturating_add(i32::from(r.height))).min(bounds_y1);
                if x1 <= x0 || y1 <= y0 {
                    return None;
                }
                Some(vk::Rect2D {
                    offset: vk::Offset2D { x: x0, y: y0 },
                    extent: vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                })
            })
            .collect();
        if vk_rects.is_empty() {
            return Ok(());
        }

        // Open frame if not already open (same pattern as fill_rect_batch).
        if !inner.frame_builder.is_open() {
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();

        // Prelude: first_touch + first_touch_drawable + touch_render_fence
        // + per-rect damage.
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(target, prior_dst_ticket);
            open.layouts.first_touch_drawable(target, dst_pre_layout);
        }
        store.touch_render_fence(target, frame_ticket.clone());
        for r in &vk_rects {
            store.damage(target, *r);
        }

        // Build RecordedLogicFill payload and append to the open frame.
        let payload = Box::new(crate::kms::render::frame_builder::RecordedLogicFill {
            dst_id: target,
            dst_image_view: image_view,
            dst_extent: extent,
            dst_format: format,
            dst_old_layout: dst_pre_layout,
            logic_mode: function,
            channels,
            color,
            rects: vk_rects,
        });
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                crate::kms::render::frame_builder::RecordedOp::LogicFill(payload),
                &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }
        store.mark_contents_modified(target);
        Ok(())
    }

    // ── Op: copy_area (Stage 2d) ────────────────────────────────

    /// Copy `src_rect` from `src` into `dst` at `dst_pos`. The
    /// disjoint case is a straight `vkCmdCopyImage`. When
    /// `src == dst`, a same-image overlap is detected and routed
    /// through a scratch-image via `vkCmdCopyImage` twice (per
    /// Stage 2 plan §"copy_area" subcase). Stage 2's slow scratch
    /// path is acceptable — apps that hit it (xterm scroll
    /// without compositor) need glyphs to be relevant anyway,
    /// landing in Stage 3.
    ///
    /// # Errors
    ///
    /// `UnknownDrawable` if either id is missing; `Vk` for
    /// any Vk failure; `NoVk` on the stub engine.
    pub(crate) fn copy_area(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        src_handle: Src,
        dst_handle: Dst,
        src_rect: vk::Rect2D,
        dst_pos: vk::Offset2D,
    ) -> Result<(), RenderError> {
        let src = src_handle.id();
        let dst = dst_handle.id();
        // Phase B.3 (N9): empty-input fast-path FIRST — before any flush.
        if src_rect.extent.width == 0 || src_rect.extent.height == 0 {
            return Ok(());
        }
        // Phase B.3 (N9): renderer_failed check before any open-frame mutation.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // Phase B.3 (N9): flush pending_render_batch at entry. May close an
        // open frame (chronological X11 ordering with pre-existing batches).
        self.flush_render_batch(store, platform, RenderFlushReason::Copy)?;

        // Preflight: read src + dst metadata + format check WITHOUT mutating
        // anything in the open frame. inner borrow is scoped.
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let (src_image, src_extent, src_format) = {
            let d = store.get(src).ok_or(RenderError::UnknownDrawable(src))?;
            (d.storage.image, d.storage.extent, d.storage.format)
        };
        let (dst_image, dst_extent, dst_format) = {
            let d = store.get(dst).ok_or(RenderError::UnknownDrawable(dst))?;
            (d.storage.image, d.storage.extent, d.storage.format)
        };
        if src_format != dst_format {
            return Err(RenderError::UnsupportedDepth(0));
        }

        // Jointly clamp the src sub-rect and its dst placement to BOTH
        // extents, keeping src↔dst aligned. Handles X11 wire negative /
        // overflow offsets in one place; the previous inline arithmetic
        // clamped the source (trimming width for a negative offset) and
        // then re-subtracted the negative dst offset, under-copying by
        // |offset| px on the trailing edge — the MATE compositor
        // slow-drag-left shadow smear.
        let Some((src_rect, dst_rect)) =
            // #133 step 3 (P4): both sides clamp to their handle's
            // bounds, so neither the read nor the write can touch a
            // bordered window's ring.
            clamp_copy_rects_to(
                src_rect,
                dst_pos,
                src_handle.bounds_in(src_extent),
                dst_handle.bounds_in(dst_extent),
            )
        else {
            return Ok(());
        };
        let copy_w = dst_rect.extent.width;
        let copy_h = dst_rect.extent.height;

        // Phase B.3 (N8): allocate self-overlap scratch FIRST, BEFORE any
        // open-frame state mutation. Allocation failure returns Err with the
        // frame untouched (no rollback needed).
        let self_overlap_scratch: Option<ScratchImage> = if src == dst {
            Some(allocate_scratch_image(
                &inner.vk.clone(),
                platform,
                copy_w,
                copy_h,
                src_format,
            )?)
        } else {
            None
        };

        // Open the frame if not already open. Phase B.2 Mechanism 2: bump
        // acquire_generation at open + capture on OpenFrame. Mirror of
        // composite_glyphs_via_frame_builder at engine.rs:5315-5323.
        if !inner.frame_builder.is_open() {
            // Release the inner borrow before calling the platform method
            // (which doesn't need it). Same `let _ = inner` pattern as line 5318.
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();

        // Prelude state: first-touch + layout overlay for BOTH dst and src
        // (per N1's single-terminal layout + ticket-touch discipline).
        let dst_pre_layout = inner.current_layout_for_drawable(store, dst);
        let src_pre_layout = if src == dst {
            dst_pre_layout
        } else {
            inner.current_layout_for_drawable(store, src)
        };
        let prior_dst_ticket = store.get(dst).and_then(|d| d.last_render_ticket.clone());
        let prior_src_ticket = if src == dst {
            prior_dst_ticket.clone()
        } else {
            store.get(src).and_then(|d| d.last_render_ticket.clone())
        };
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(dst, prior_dst_ticket);
            open.layouts.first_touch_drawable(dst, dst_pre_layout);
            if src != dst {
                open.touched.first_touch(src, prior_src_ticket);
                open.layouts.first_touch_drawable(src, src_pre_layout);
            }
        }
        store.touch_render_fence(dst, frame_ticket.clone());
        if src != dst {
            store.touch_render_fence(src, frame_ticket.clone());
        }
        store.damage(dst, dst_rect);

        // Phase B.3 (N1 + N8): append the op + set BOTH dst and src overlays
        // to SHADER_READ_ONLY_OPTIMAL (single-terminal-layout rule). For
        // self-overlap (src == dst), only one entry needed (idempotent).
        let payload = Box::new(crate::kms::render::frame_builder::RecordedCopyArea {
            dst_id: dst,
            src_id: src,
            src_rect,
            dst_rect,
            src_format,
            src_extent,
            dst_extent,
            src_image,
            dst_image,
            src_old_layout: src_pre_layout,
            dst_old_layout: dst_pre_layout,
            self_overlap_scratch,
        });
        let layout_updates: &[(DrawableId, vk::ImageLayout)] = if src == dst {
            &[(dst, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        } else {
            &[
                (dst, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                (src, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            ]
        };
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                crate::kms::render::frame_builder::RecordedOp::CopyArea(payload),
                layout_updates,
            );
        }
        store.mark_contents_modified(dst);
        Ok(())
    }

    // ── Op: masked_copy_area (GPU-side clip) ────────────────────

    /// Append a `RecordedOp::MaskedCopyArea`: copy `src_pos`+`extent` from
    /// `src` into `dst` at `dst_pos`, but masked per-texel by an R8 clip
    /// `mask`. Mirrors [`Self::copy_area`]'s prelude (clamp/project,
    /// self-overlap scratch, first-touch, ticket, layout overlay, damage)
    /// but the DRAW samples both the source and the mask.
    ///
    /// The mask source ([`MaskedCopyMask`]) is the GC-owned snapshot in
    /// production (Phase 2) or a plain depth-1 drawable in the exactness
    /// tests; either way the recorded op only SAMPLES it. The mask's
    /// layout/ticket are NOT engine-drawable-keyed here (snapshot first-touch
    /// for rollback lands in Task 12 when `mask.snapshot_id` is `Some`).
    ///
    /// # Errors
    ///
    /// `RendererFailed` if the renderer has already failed; `UnknownDrawable`
    /// if `src`/`dst` is missing; `Vk` for any Vk failure; `NoVk` on the stub
    /// engine.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn masked_copy_area(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        src_handle: Src,
        dst_handle: Dst,
        src_pos: vk::Offset2D,
        dst_pos: vk::Offset2D,
        extent: vk::Extent2D,
        mask: MaskedCopyMask,
        scissors: &[vk::Rect2D],
    ) -> Result<(), RenderError> {
        let src = src_handle.id();
        let dst = dst_handle.id();
        // Empty-input fast-path FIRST — before any flush (mirror copy_area N9).
        if extent.width == 0 || extent.height == 0 {
            return Ok(());
        }
        // renderer_failed check before any open-frame mutation.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // Flush pending_render_batch at entry. May close an open frame
        // (chronological X11 ordering with pre-existing batches).
        self.flush_render_batch(store, platform, RenderFlushReason::Copy)?;

        // Lazy-init RENDER assets — the deferred masked_blit replay
        // (`emit_recorded_masked_copyarea_into_cb`) needs `inner.masked_blit`
        // present at frame-close time. Mirrors the render-pass ops
        // (render_composite / traps) which call this before recording; the
        // masked-blit pipeline is built here on first use.
        self.ensure_render_assets(platform)?;

        // Preflight: read src + dst metadata WITHOUT mutating the open frame.
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let (src_image, src_view, src_extent, src_format) = {
            let d = store.get(src).ok_or(RenderError::UnknownDrawable(src))?;
            (
                d.storage.image,
                d.storage.image_view,
                d.storage.extent,
                d.storage.format,
            )
        };
        let (dst_image, dst_view, dst_extent, dst_format) = {
            let d = store.get(dst).ok_or(RenderError::UnknownDrawable(dst))?;
            (
                d.storage.image,
                d.storage.image_view,
                d.storage.extent,
                d.storage.format,
            )
        };

        // Jointly clamp src sub-rect + dst placement to both extents,
        // keeping them aligned (shared with copy_area; fixes the
        // negative-offset double-subtract under-copy).
        let Some((src_rect, dst_rect)) = clamp_copy_rects_to(
            vk::Rect2D {
                offset: src_pos,
                extent,
            },
            dst_pos,
            // #133 step 3 (P4) — content bounds, not raw extents.
            src_handle.bounds_in(src_extent),
            dst_handle.bounds_in(dst_extent),
        ) else {
            return Ok(());
        };
        let copy_w = dst_rect.extent.width;
        let copy_h = dst_rect.extent.height;
        // src_texel = dst_pixel + copy_offset (non-overlap sample-space offset).
        let copy_offset = [
            src_rect.offset.x - dst_rect.offset.x,
            src_rect.offset.y - dst_rect.offset.y,
        ];

        // N8: allocate self-overlap scratch FIRST, BEFORE any open-frame state
        // mutation. Allocation failure returns Err with the frame untouched.
        let self_overlap_scratch: Option<SampledScratchImage> = if src == dst {
            Some(allocate_sampled_scratch_image(
                &inner.vk.clone(),
                copy_w,
                copy_h,
                src_format,
            )?)
        } else {
            None
        };
        // The op keeps the LIVE src (`src_image`/`src_pre_layout`) for the copy
        // + barrier. The DRAW samples `sample_view`/`sample_extent` with
        // `eff_copy_offset`. On self-overlap, sampling the scratch (region at
        // (0,0)) means sample_view=scratch.view and src_texel = dst_pixel −
        // dst_rect.offset; otherwise it samples the src identity view directly.
        let (sample_view, sample_extent, eff_copy_offset) =
            if let Some(s) = self_overlap_scratch.as_ref() {
                (
                    s.view,
                    vk::Extent2D {
                        width: copy_w,
                        height: copy_h,
                    },
                    [-dst_rect.offset.x, -dst_rect.offset.y],
                )
            } else {
                (src_view, src_extent, copy_offset)
            };

        // Open the frame if not already open. Mirror copy_area: bump
        // acquire_generation at open + capture on OpenFrame.
        if !inner.frame_builder.is_open() {
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();

        // Prelude state: first-touch + layout overlay for BOTH dst and src.
        // dst is a write; src is a read. The mask snapshot is NOT a drawable
        // participant here (engine-managed; first-touch for rollback is
        // recorded in Task 12 when snapshot_id is Some).
        let dst_pre_layout = inner.current_layout_for_drawable(store, dst);
        let src_pre_layout = if src == dst {
            dst_pre_layout
        } else {
            inner.current_layout_for_drawable(store, src)
        };
        let prior_dst_ticket = store.get(dst).and_then(|d| d.last_render_ticket.clone());
        let prior_src_ticket = if src == dst {
            prior_dst_ticket.clone()
        } else {
            store.get(src).and_then(|d| d.last_render_ticket.clone())
        };
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(dst, prior_dst_ticket);
            open.layouts.first_touch_drawable(dst, dst_pre_layout);
            if src != dst {
                open.touched.first_touch(src, prior_src_ticket);
                open.layouts.first_touch_drawable(src, src_pre_layout);
            }
        }
        // Phase 2 clip Task 12: snapshot first-touch for rollback. Only when
        // the mask is a GC-owned snapshot (production path); the Phase-1
        // plain-drawable test path passes `snapshot_id: None`.
        if let Some(sid) = mask.snapshot_id {
            snapshot_first_touch(inner, sid);
        }
        store.touch_render_fence(dst, frame_ticket.clone());
        if src != dst {
            store.touch_render_fence(src, frame_ticket.clone());
        }
        store.damage(dst, dst_rect);

        // Build the op + set BOTH dst and src overlays to SHADER_READ (single-
        // terminal-layout rule). For self-overlap, one entry (idempotent).
        let payload = Box::new(crate::kms::render::frame_builder::RecordedMaskedCopyArea {
            dst_id: dst,
            src_id: src,
            dst_format,
            dst_image,
            dst_view,
            dst_extent,
            // LIVE src drawable (copy + barrier); SAMPLED view/extent for draw.
            src_image,
            src_old_layout: src_pre_layout,
            live_src_offset: [src_rect.offset.x, src_rect.offset.y],
            sample_view,
            sample_extent,
            mask_image: mask.image,
            mask_view: mask.view,
            mask_extent: mask.extent,
            clip_origin: mask.clip_origin,
            copy_offset: eff_copy_offset,
            dst_rect,
            // #133 step 3 (P4): the caller's scissors are already
            // content-clipped through `PaintTarget`, but fold the bounds
            // in here too so the guarantee is the engine's, not the
            // caller's. Identity when `bounds` is the whole storage.
            scissors: scissors
                .iter()
                .map(|r| clamp_rect_to(*r, dst_handle.bounds_in(dst_extent)))
                .filter(|r| r.extent.width != 0 && r.extent.height != 0)
                .collect(),
            dst_old_layout: dst_pre_layout,
            mask_old_layout: mask.old_layout,
            self_overlap_scratch,
        });
        let layout_updates: &[(DrawableId, vk::ImageLayout)] = if src == dst {
            &[(dst, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        } else {
            &[
                (dst, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                (src, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            ]
        };
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                crate::kms::render::frame_builder::RecordedOp::MaskedCopyArea(payload),
                layout_updates,
            );
        }
        // Phase 2 clip Task 12: commit the snapshot's terminal state on the
        // SAMPLE path. The masked-blit DRAW samples the snapshot, leaving it in
        // SHADER_READ_ONLY_OPTIMAL and bound to this frame's ticket. Do NOT
        // touch `snapshotted_version` — the SAMPLE path reads, it does not
        // (re)populate; the version-advancing commit lives on the WRITE path in
        // `refresh_clip_snapshot` (Task 13). On close-failure `rollback_snapshots`
        // restores all three fields from `snapshot_touch`.
        if let Some(sid) = mask.snapshot_id
            && let Some(snap) = inner.clip_snapshots.get_mut(&sid)
        {
            snap.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
            snap.last_render_ticket = Some(frame_ticket.clone());
        }
        store.mark_contents_modified(dst);
        Ok(())
    }

    // ── Op: cow_copy_area (Stage 5 Task 3 POC) ──────────────────

    /// Phase B.3 (N3, N9, N10): coalescing variant of [`Self::copy_area`]
    /// for the Composite Overlay Window. Per N3, the cow is a regular
    /// [`DrawableId`] registered in the store like any other drawable;
    /// this function forwards to [`Self::copy_area`] directly.
    ///
    /// Same-image overlap (`src == cow_id`) is defended with an explicit
    /// error (legacy invariant at engine.rs:3109-3111 preserved).
    ///
    /// The frame-builder's per-frame collapse (multiple ops in one
    /// submitted CB) provides COW coalescing.
    ///
    /// # Errors
    ///
    /// `UnknownDrawable` if `cow_id` or `src` is missing;
    /// `RendererFailed` if the renderer has already failed; `Vk`
    /// for any Vk failure; `NoVk` on the stub engine.
    pub(crate) fn cow_copy_area(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        cow_id: Dst,
        src: Src,
        src_rect: vk::Rect2D,
        dst_pos: vk::Offset2D,
    ) -> Result<(), RenderError> {
        // N9: empty-input fast-path FIRST.
        if src_rect.extent.width == 0 || src_rect.extent.height == 0 {
            return Ok(());
        }
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // N9: flush pending_render_batch at entry.
        self.flush_render_batch(store, platform, RenderFlushReason::Copy)?;

        // Sanity: same-image overlap not handled on the cow path
        // (legacy invariant at engine.rs:3109-3111). The regular
        // copy_area's self-overlap scratch path would handle it, but
        // cow workloads never have src == cow_id in practice.
        if src.id() == cow_id.id() {
            return Err(RenderError::UnsupportedDepth(0));
        }

        // Per N3: cow_id is a regular DrawableId — forward to copy_area.
        // The frame builder's per-frame collapse provides coalescing.
        self.copy_area(store, platform, src, cow_id, src_rect, dst_pos)
    }
}

/// #133 step 3 (P4) — clamp `rect` to an arbitrary bounds RECT rather
/// than to `[0, extent)`. With storage-inclusive borders the drawable's
/// usable area is the content rect inside its storage
/// (`compAllocPixmap`, `composite/compalloc.c:610`), so every site that
/// clamped to the storage extent now clamps to the destination handle's
/// bounds. `clamp_rect(r, extent)` is exactly
/// `clamp_rect_to(r, {(0, 0), extent})`.
pub(crate) fn clamp_rect_to(rect: vk::Rect2D, bounds: vk::Rect2D) -> vk::Rect2D {
    let min_x = bounds.offset.x;
    let min_y = bounds.offset.y;
    let max_x = bounds.offset.x.saturating_add_unsigned(bounds.extent.width);
    let max_y = bounds
        .offset
        .y
        .saturating_add_unsigned(bounds.extent.height);
    let x0 = rect.offset.x.clamp(min_x, max_x);
    let y0 = rect.offset.y.clamp(min_y, max_y);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .clamp(min_x, max_x);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .clamp(min_y, max_y);
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
            height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
        },
    }
}

/// #133 step 3 (P4) — resolve a recorded `Option<bounds>` against the
/// storage extent it will be applied in. `None` (every pixmap, every
/// `bw == 0` window) yields the full extent, i.e. the pre-#133 value.
pub(crate) fn resolve_recorded_bounds(
    bounds: Option<vk::Rect2D>,
    extent: vk::Extent2D,
) -> vk::Rect2D {
    match bounds {
        Some(b) => clamp_rect(b, extent),
        None => vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        },
    }
}

pub(crate) fn clamp_rect(rect: vk::Rect2D, extent: vk::Extent2D) -> vk::Rect2D {
    let max_x = i32::try_from(extent.width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(extent.height).unwrap_or(i32::MAX);
    let x0 = rect.offset.x.max(0).min(max_x);
    let y0 = rect.offset.y.max(0).min(max_y);
    let x1 = rect
        .offset
        .x
        .saturating_add_unsigned(rect.extent.width)
        .clamp(0, max_x);
    let y1 = rect
        .offset
        .y
        .saturating_add_unsigned(rect.extent.height)
        .clamp(0, max_y);
    vk::Rect2D {
        offset: vk::Offset2D { x: x0, y: y0 },
        extent: vk::Extent2D {
            width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
            height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
        },
    }
}

/// Jointly clamp a `CopyArea` source sub-rect and its destination
/// placement to BOTH drawables' bounds, keeping src↔dst aligned.
///
/// `src_rect` is the requested source sub-rect (its `offset` may be
/// negative or overflow `src_extent`); `dst_pos` is where its origin
/// lands in the destination (may be negative or overflow
/// `dst_extent`). Pixel `src(src_rect.offset + i)` maps to
/// `dst(dst_pos + i)`, so any column/row skipped for being out of
/// bounds on EITHER side must advance BOTH origins by the same amount.
/// Returns aligned `(src, dst)` rects that share one extent (the
/// surviving overlap), or `None` if nothing is visible.
///
/// Replaces the previous per-call inline arithmetic in `copy_area` /
/// `masked_copy_area`, which clamped the source with [`clamp_rect`]
/// (already trimming width/height for a negative source offset) and
/// then re-subtracted the negative `dst_pos` — double-counting the
/// offset and under-copying by `|offset|` px on the trailing edge
/// (the MATE compositor slow-drag-left shadow smear).
pub(super) fn clamp_copy_rects(
    src_rect: vk::Rect2D,
    dst_pos: vk::Offset2D,
    src_extent: vk::Extent2D,
    dst_extent: vk::Extent2D,
) -> Option<(vk::Rect2D, vk::Rect2D)> {
    let whole = |extent| vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent,
    };
    clamp_copy_rects_to(src_rect, dst_pos, whole(src_extent), whole(dst_extent))
}

/// #133 step 3 (P4) — [`clamp_copy_rects`] against arbitrary source and
/// destination bounds rects. Clipping the SOURCE side is what stops a
/// `CopyArea` reading a bordered window's ring back as content; clipping
/// the destination side is what stops it writing one. Both sides stay
/// aligned because a row/column dropped on either side advances both
/// origins.
pub(super) fn clamp_copy_rects_to(
    src_rect: vk::Rect2D,
    dst_pos: vk::Offset2D,
    src_bounds: vk::Rect2D,
    dst_bounds: vk::Rect2D,
) -> Option<(vk::Rect2D, vk::Rect2D)> {
    // Work in the shared index space `i` where pixel `i` is
    // `src(so + i)` == `dst(do + i)`. A column/row is visible only if
    // it is in bounds on BOTH sides, so intersect all four half-open
    // ranges. Advancing the low end skips off-screen leading pixels on
    // whichever side needs it (negative src OR negative dst); the high
    // end clamps to whichever drawable's trailing edge is nearer.
    let so_x = i64::from(src_rect.offset.x);
    let so_y = i64::from(src_rect.offset.y);
    let do_x = i64::from(dst_pos.x);
    let do_y = i64::from(dst_pos.y);
    let w = i64::from(src_rect.extent.width);
    let h = i64::from(src_rect.extent.height);
    let s_x0 = i64::from(src_bounds.offset.x);
    let s_y0 = i64::from(src_bounds.offset.y);
    let d_x0 = i64::from(dst_bounds.offset.x);
    let d_y0 = i64::from(dst_bounds.offset.y);
    let s_x1 = s_x0 + i64::from(src_bounds.extent.width);
    let s_y1 = s_y0 + i64::from(src_bounds.extent.height);
    let d_x1 = d_x0 + i64::from(dst_bounds.extent.width);
    let d_y1 = d_y0 + i64::from(dst_bounds.extent.height);

    let i_lo = 0.max(s_x0 - so_x).max(d_x0 - do_x);
    let i_hi = w.min(s_x1 - so_x).min(d_x1 - do_x);
    let j_lo = 0.max(s_y0 - so_y).max(d_y0 - do_y);
    let j_hi = h.min(s_y1 - so_y).min(d_y1 - do_y);
    let copy_w = i_hi - i_lo;
    let copy_h = j_hi - j_lo;
    if copy_w <= 0 || copy_h <= 0 {
        return None;
    }
    // i_lo/j_lo ∈ [0, extent] and offsets are i16-range wire values, so
    // every sum below is well within i32.
    let extent = vk::Extent2D {
        width: u32::try_from(copy_w).unwrap_or(0),
        height: u32::try_from(copy_h).unwrap_or(0),
    };
    Some((
        vk::Rect2D {
            offset: vk::Offset2D {
                x: i32::try_from(so_x + i_lo).unwrap_or(0),
                y: i32::try_from(so_y + j_lo).unwrap_or(0),
            },
            extent,
        },
        vk::Rect2D {
            offset: vk::Offset2D {
                x: i32::try_from(do_x + i_lo).unwrap_or(0),
                y: i32::try_from(do_y + j_lo).unwrap_or(0),
            },
            extent,
        },
    ))
}
