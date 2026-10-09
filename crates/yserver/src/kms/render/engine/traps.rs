use super::*;

impl RenderEngine {
    /// Stage 3e.2: lazy-init trap pipeline + mask scratch. Idempotent.
    /// Called by `render_traps_or_tris` on first use. The mask
    /// scratch starts at the default extent and grows via
    /// `ensure_image_size_returning_old` per call; the pipeline is
    /// built once at the standard R8_UNORM mask format.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `Vk(...)` for pipeline / scratch construction failure.
    fn ensure_trap_assets(&mut self, platform: &PlatformBackend) -> Result<(), RenderError> {
        use crate::kms::vk::{mask_scratch::MaskScratch, trap_pipeline::TrapPipeline};
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        if inner.trap_pipeline.is_none() {
            let p =
                TrapPipeline::new(Arc::clone(&inner.vk), vk::Format::R8_UNORM).map_err(|e| {
                    log::error!("render ensure_trap_assets: TrapPipeline::new failed: {e:?}");
                    RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                })?;
            inner.trap_pipeline = Some(p);
        }
        if inner.mask_scratch.is_none() {
            let s = MaskScratch::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("render ensure_trap_assets: MaskScratch::new failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            inner.mask_scratch = Some(s);
        }
        Ok(())
    }

    // ── Op: render_traps_or_tris (Stage 3e.2) ───────────────────

    /// GPU-rasterized RENDER `Trapezoids` / `Triangles`. Backend
    /// wrapper decodes the wire stream, applies `(x_off, y_off)`,
    /// computes the bounding box, and packs per-instance vertex
    /// data; the engine method takes those pre-cooked inputs and
    /// drives a two-stage CB: first the trap pipeline rasterizes
    /// analytic edge coverage into an R8 [`MaskScratch`] image,
    /// then the standard render pipeline composites `src ⊗ mask`
    /// into `dst`. Mirrors v1's `try_vk_render_traps_or_tris`
    /// (kms/backend.rs:4500) port — same trap pipeline + mask
    /// scratch infrastructure, adapted for v2's per-op CB shape.
    ///
    /// `bbox` is `(x, y, w, h)` in pixel coords (already clamped
    /// to non-negative by the wrapper). `prim_kind` selects which
    /// sibling pipeline to bind (trap edges vs triangle edges).
    ///
    /// Out-of-scope gating (unknown op, gradient src — Stage 3e
    /// gradient support hasn't landed yet, mask self-alias,
    /// unsupported dst format, src self-alias) returns
    /// `Ok(stats)` with `recorded_draws = 0` — same shape as
    /// `render_composite`. Source self-alias bails with a gap log
    /// (would need scratch routing à la 3c.3; rare in real-world
    /// trap workloads).
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` if `dst_id` is missing.
    /// - `Vk(...)` for pipeline / scratch / CB failures.
    /// - `RendererFailed` if `platform.renderer_failed`.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_traps_or_tris(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        dst: Dst,
        prim_kind: TrapPrimKind,
        instance_data: &[u8],
        instance_count: u32,
        bbox: (i32, i32, u32, u32),
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        src_transform: Option<PictTransform>,
        // Client xSrc/ySrc source-sampling origin, already shifted by
        // the caller's dst redirect / x_off delta (xSrc-dx, ySrc-dy).
        // Recorded into the payload; the emit folds in bbox for the
        // non-full-dst branch. Fixes RENDER Trapezoids/Triangles
        // dropping xSrc/ySrc (GTK CSD shadow blur-mask sampling).
        src_origin_x: i32,
        src_origin_y: i32,
        // Audit #4 (2026-05-19) — src/dst PictFormat IDs. Mirrors the
        // render_composite wiring: pict_format-aware α swizzle on the
        // sample view, pict_format-aware dst_has_alpha for pipeline +
        // readback selection. 0 = no Picture context → legacy depth
        // heuristic (the codex round 2026-05-19 follow-up to audit
        // #4 closed the trap/tri path that was originally missed).
        src_pict_format: u32,
        dst_pict_format: u32,
    ) -> Result<CompositeStats, RenderError> {
        use crate::kms::vk::render_pipeline::StdPictOp;

        let dst_id = dst.id();
        // Phase B.3 (N5 + N9): empty-input fast-path — BEFORE
        // flush_render_batch and any other state mutation.
        let mut stats = CompositeStats::default();
        if instance_count == 0 {
            return Ok(stats);
        }
        let (bbox_x, bbox_y, bbox_w, bbox_h) = bbox;
        if bbox_w == 0 || bbox_h == 0 {
            return Ok(stats);
        }

        // N9 order: renderer_failed check.
        {
            let _inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
            if platform.renderer_failed {
                return Err(RenderError::RendererFailed);
            }
        }

        // N9 order: flush_render_batch before any state mutation.
        self.flush_render_batch(store, platform, RenderFlushReason::Traps)?;

        // Lazy-init RENDER + TRAP assets (idempotent, preserves legacy).
        self.ensure_render_assets(platform)?;
        self.ensure_trap_assets(platform)?;

        // Preflight: resolve dst metadata.
        let (dst_image, dst_view, dst_extent, dst_format, dst_depth) = {
            let _inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
            let d = store
                .get(dst_id)
                .ok_or(RenderError::UnknownDrawable(dst_id))?;
            (
                d.storage.image,
                d.storage.image_view,
                d.storage.extent,
                d.storage.format,
                d.depth,
            )
        };
        if dst_extent.width == 0 || dst_extent.height == 0 {
            return Ok(stats);
        }
        if !matches!(
            dst_format,
            vk::Format::B8G8R8A8_UNORM | vk::Format::R8_UNORM
        ) {
            log::debug!("render render_traps_or_tris gap: dst format {dst_format:?} unsupported");
            return Ok(stats);
        }
        // Audit #4 (2026-05-19): pict_format-aware dst alpha.
        let dst_has_alpha = dst_has_alpha_for_pict_format(dst_format, dst_depth, dst_pict_format);
        let Some(std_op) = StdPictOp::from_u8(op) else {
            log::debug!("render render_traps_or_tris gap: unsupported op {op}");
            return Ok(stats);
        };
        let needs_dst_readback = std_op.needs_dst_readback();

        // Self-alias gate (preserve legacy at engine.rs:7271-7274).
        if matches!(src, ResolvedSource::Drawable(sd) if sd.id() == dst_id) {
            log::debug!("render render_traps_or_tris gap: src self-alias (out of scope for 3e.2)");
            return Ok(stats);
        }

        // Step 6 (Phase 9A for mask_scratch): peek + close-before-grow + grow
        // + adopt. Mirrors render_composite_via_frame_builder at engine.rs:6539-6557.
        let need_grow_mask = {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .mask_scratch
                .as_ref()
                .map(|s| !s.fits(bbox_w, bbox_h))
                .unwrap_or(true)
        };
        if need_grow_mask
            && self
                .inner
                .as_ref()
                .expect("inner")
                .frame_builder
                .open
                .as_ref()
                .is_some_and(|o| o.has_recorded_work())
        {
            self.close_open_frame(
                store,
                platform,
                crate::kms::render::frame_builder::CloseReason::ScratchGrow,
            )?;
        }
        if need_grow_mask {
            let retired = {
                let inner = self.inner.as_mut().expect("inner");
                inner
                    .mask_scratch
                    .as_mut()
                    .expect("ensured")
                    .ensure_image_size_returning_old(bbox_w, bbox_h)
                    .map_err(|e| {
                        log::warn!("render render_traps_or_tris: mask ensure_image_size: {e:?}");
                        RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                    })?
            };
            let inner = self.inner.as_mut().expect("inner");
            inner.adopt_retired_resource_for_gpu_retirement(retired);
        }

        // Step 7 (Phase 9A for dst_readback): peek + close-before-grow + grow
        // + adopt when std_op.needs_dst_readback().
        if needs_dst_readback {
            let need_grow_rb = {
                let inner = self.inner.as_ref().expect("inner");
                inner
                    .dst_readback
                    .as_ref()
                    .map(|rb| !rb.fits(dst_format, dst_extent.width, dst_extent.height))
                    .unwrap_or(true)
            };
            if need_grow_rb
                && self
                    .inner
                    .as_ref()
                    .expect("inner")
                    .frame_builder
                    .open
                    .as_ref()
                    .is_some_and(|o| o.has_recorded_work())
            {
                self.close_open_frame(
                    store,
                    platform,
                    crate::kms::render::frame_builder::CloseReason::ScratchGrow,
                )?;
            }
            if need_grow_rb {
                let retired = {
                    let inner = self.inner.as_mut().expect("inner");
                    inner
                        .dst_readback
                        .as_mut()
                        .expect("ensured")
                        .ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                        .map_err(|e| {
                            log::warn!("render render_traps_or_tris: dst readback ensure: {e:?}");
                            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                        })?
                };
                let inner = self.inner.as_mut().expect("inner");
                inner.adopt_retired_resource_for_gpu_retirement(retired);
            }
        }

        // Step 8: resolve append-time-stable fields per N5.

        // src_kind: match ResolvedSource variants.
        let src_kind = {
            let inner = self.inner.as_ref().expect("inner");
            match src {
                ResolvedSource::Drawable(sd) => {
                    let id = sd.id();
                    let info = drawable_for_render_view(store, id)
                        .ok_or(RenderError::UnknownDrawable(id))?;
                    let swizzle_class =
                        swizzle_class_for_pict_format(info.format, info.depth, src_pict_format);
                    // #133 step 3 (P4): the picture drawable's content
                    // origin travels with the recorded source so the
                    // composite stage samples the window's content, not
                    // its border ring. `(0, 0)` for pixmaps / bw == 0.
                    crate::kms::render::frame_builder::RecordedTrapSrcKind::Drawable {
                        id,
                        swizzle_class,
                        sample_offset: sd.offset(),
                    }
                }
                ResolvedSource::Solid(color) => {
                    crate::kms::render::frame_builder::RecordedTrapSrcKind::Solid(color)
                }
                ResolvedSource::Gradient(xid) => match inner.picture_paint.get(&xid) {
                    Some(PicturePaintState::Gradient(g)) => {
                        // B.3 hotfix 2: clone the Arc so the recorded op holds
                        // a strong ref past picture_paint_remove. The clone is
                        // moved to pins.retired_resources at close time so it
                        // survives until the GPU fence fires.
                        let picture = g.clone();
                        let intrinsic_axis_projection = picture.axis_projection();
                        crate::kms::render::frame_builder::RecordedTrapSrcKind::Gradient {
                            picture,
                            intrinsic_axis_projection,
                        }
                    }
                    None => {
                        log::debug!(
                            "render render_traps_or_tris gap: gradient picture 0x{xid:x} \
                             missing from engine.picture_paint (LUT build likely failed)"
                        );
                        return Ok(stats);
                    }
                },
                ResolvedSource::None => {
                    log::debug!("render render_traps_or_tris gap: src None");
                    return Ok(stats);
                }
            }
        };

        // src_extent: needed for CompositeAttrs at emit time.
        let src_extent = match &src_kind {
            crate::kms::render::frame_builder::RecordedTrapSrcKind::Drawable { id, .. } => {
                drawable_for_render_view(store, *id)
                    .map(|info| info.extent)
                    .unwrap_or(vk::Extent2D {
                        width: 1,
                        height: 1,
                    })
            }
            crate::kms::render::frame_builder::RecordedTrapSrcKind::Solid(_) => vk::Extent2D {
                width: 1,
                height: 1,
            },
            crate::kms::render::frame_builder::RecordedTrapSrcKind::Gradient {
                picture, ..
            } => {
                // B.3 hotfix 2: extent is on the Arc clone; no HashMap lookup.
                picture.extent()
            }
        };
        let src_is_synthetic_1x1 = matches!(
            src_kind,
            crate::kms::render::frame_builder::RecordedTrapSrcKind::Solid(_)
        );

        // src_repeat: pre-resolve via repeat_to_shader_const.
        // The payload stores it as u32 (matches the spec field type); cast
        // from the helper's i32 return — the shader constants are 0..=3 and
        // always non-negative so the cast is lossless.
        #[allow(clippy::cast_sign_loss)]
        let src_repeat_const = crate::kms::backend::repeat_to_shader_const(src_repeat) as u32;

        // src_force_opaque via pict_format-aware helper.
        let src_force_opaque = resolve_force_opaque_pict_format(store, &src, src_pict_format);

        // user_src_xform via pixman_transform_to_affine.
        let user_src_xform =
            crate::kms::backend::pixman_transform_to_affine(src_transform.as_ref(), src_extent);

        // needs_full_dst byte-pattern test (engine.rs:7472).
        let needs_full_dst = matches!(op, 0 | 1 | 5 | 6 | 7 | 10 | 13 | 16..=27 | 32..=43);
        let (render_dst_x, render_dst_y, render_w, render_h) = if needs_full_dst {
            (0, 0, dst_extent.width, dst_extent.height)
        } else {
            (bbox_x, bbox_y, bbox_w, bbox_h)
        };

        // clip_scissors: pre-clamped at append. #133 step 3 (P4): the
        // inline copy of `build_render_clip_scissors` was replaced by the
        // shared bounds-aware helper, and the no-picture-clip case is
        // additionally intersected with the destination's content bounds
        // — a trapezoid or triangle edge outside the content rect is a
        // border-ring write otherwise.
        let dst_bounds = dst.bounds_in(dst_extent);
        let clip_scissors: Vec<vk::Rect2D> = match clip_rects {
            None => {
                let render_rect = vk::Rect2D {
                    offset: vk::Offset2D {
                        x: render_dst_x,
                        y: render_dst_y,
                    },
                    extent: vk::Extent2D {
                        width: render_w,
                        height: render_h,
                    },
                };
                // Only the bordered case narrows here: with no content
                // clip the scissor stays the render rect verbatim, as it
                // was before #133 (the render rect is either the full dst
                // or the caller's bbox).
                if dst.bounds().is_none() {
                    vec![render_rect]
                } else {
                    let clipped = clamp_rect_to(render_rect, dst_bounds);
                    if clipped.extent.width == 0 || clipped.extent.height == 0 {
                        return Ok(stats);
                    }
                    vec![clipped]
                }
            }
            Some(cr) => {
                let out = build_render_clip_scissors_to(Some(cr), dst_bounds);
                if out.is_empty() {
                    return Ok(stats);
                }
                out
            }
        };

        // Step 9: open frame if not open.
        {
            let inner = self.inner.as_mut().expect("inner");
            if !inner.frame_builder.is_open() {
                let _ = inner;
                let ticket = platform.submit_group_ticket_or_open()?;
                let inner = self.inner.as_mut().expect("inner");
                inner.acquire_generation = inner.acquire_generation.saturating_add(1);
                let frame_generation = inner.acquire_generation;
                inner.frame_builder.open_for_paint(ticket, frame_generation);
            }
        }

        // Snapshot frame ticket.
        let frame_ticket = {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .frame_builder
                .open
                .as_ref()
                .expect("just opened")
                .ticket
                .clone()
        };

        // Step 10: upload + pin the vertex data into the frame just
        // opened (#177 upload arena). Done here, after the last point the
        // frame can close (the scratch-grow close above), because the pin
        // belongs to the frame whose blocks hold the bytes. Before any
        // other open-frame state mutation, so an allocation failure leaves
        // the frame with no half-recorded op.
        let vertex_pin = self.inner.as_mut().expect("inner").upload_to_frame(
            instance_data,
            UPLOAD_VERTEX_ALIGN,
            crate::kms::vk::mem_accounting::ChurnClass::Traps,
        )?;

        // Step 11 (codex round-9 CRITICAL): prelude state for ALL TOUCHED DRAWABLES.
        // dst: first_touch + first_touch_drawable + touch_render_fence.
        let prior_dst_ticket = store.get(dst_id).and_then(|d| d.last_render_ticket.clone());
        let dst_pre_layout = {
            let inner = self.inner.as_ref().expect("inner");
            inner.current_layout_for_drawable(store, dst_id)
        };
        {
            let inner = self.inner.as_mut().expect("inner");
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.touched.first_touch(dst_id, prior_dst_ticket);
            open.layouts.first_touch_drawable(dst_id, dst_pre_layout);
        }
        store.touch_render_fence(dst_id, frame_ticket.clone());

        // src (only when Drawable): SAME three mutations on the src DrawableId.
        // Skipping these is a lifetime bug per codex round-9 CRITICAL.
        if let crate::kms::render::frame_builder::RecordedTrapSrcKind::Drawable {
            id: src_id, ..
        } = src_kind
        {
            let prior_src_ticket = store.get(src_id).and_then(|d| d.last_render_ticket.clone());
            let src_pre_layout = {
                let inner = self.inner.as_ref().expect("inner");
                inner.current_layout_for_drawable(store, src_id)
            };
            {
                let inner = self.inner.as_mut().expect("inner");
                let open = inner.frame_builder.open.as_mut().expect("just opened");
                open.touched.first_touch(src_id, prior_src_ticket);
                open.layouts.first_touch_drawable(src_id, src_pre_layout);
            }
            store.touch_render_fence(src_id, frame_ticket.clone());
        }

        // Damage bookkeeping (coarse, matches legacy shape).
        let dmg = vk::Rect2D {
            offset: vk::Offset2D {
                x: render_dst_x,
                y: render_dst_y,
            },
            extent: vk::Extent2D {
                width: render_w,
                height: render_h,
            },
        };
        store.damage(dst_id, clamp_rect_to(dmg, dst_bounds));
        stats.recorded_draws = u32::try_from(clip_scissors.len()).unwrap_or(u32::MAX);
        stats.used_dst_readback = needs_dst_readback;

        // Step 12: push_op_and_set_layouts. layouts_to_set includes
        // (dst, SHADER_READ_ONLY_OPTIMAL) always, AND (src, SHADER_READ_ONLY_OPTIMAL)
        // when src is Drawable.
        let payload = Box::new(
            crate::kms::render::frame_builder::RecordedRenderTrapsOrTris {
                dst_id,
                dst_image,
                dst_view,
                dst_old_layout: dst_pre_layout,
                dst_extent,
                dst_format,
                dst_has_alpha,
                std_op,
                op_byte: op,
                src_kind,
                src_extent,
                src_is_synthetic_1x1,
                src_repeat: src_repeat_const,
                src_force_opaque,
                user_src_xform,
                src_origin_x,
                src_origin_y,
                prim_kind,
                bbox_x,
                bbox_y,
                bbox_w,
                bbox_h,
                instance_count,
                clip_scissors,
                vertex_pin,
            },
        );
        {
            let inner = self.inner.as_mut().expect("inner");
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            // Build the layouts_to_set slice. dst always; src when Drawable.
            if let crate::kms::render::frame_builder::RecordedTrapSrcKind::Drawable {
                id: src_id,
                ..
            } = payload.src_kind
            {
                open.push_op_and_set_layouts(
                    crate::kms::render::frame_builder::RecordedOp::RenderTrapsOrTris(payload),
                    &[
                        (dst_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                        (src_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
                    ],
                );
            } else {
                open.push_op_and_set_layouts(
                    crate::kms::render::frame_builder::RecordedOp::RenderTrapsOrTris(payload),
                    &[(dst_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
                );
            }
        }
        store.mark_contents_modified(dst_id);

        Ok(stats)
    }
}
