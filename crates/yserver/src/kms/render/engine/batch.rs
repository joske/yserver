use super::*;

impl RenderFlushReason {
    /// Increment the matching per-second counter. Called once per
    /// real flush (an open batch was present and taken).
    fn record(self) {
        match self {
            Self::KeyChangeSameDst => {
                crate::vk_count!(rpflush_key_change_same_dst);
            }
            Self::KeyChangeDiffDst => {
                crate::vk_count!(rpflush_key_change_diff_dst);
            }
            Self::Fill => {
                crate::vk_count!(rpflush_for_fill);
            }
            Self::Copy => {
                crate::vk_count!(rpflush_for_copy);
            }
            Self::Glyph => {
                crate::vk_count!(rpflush_for_glyph);
            }
            Self::Traps => {
                crate::vk_count!(rpflush_for_traps);
            }
            Self::PutImage => {
                crate::vk_count!(rpflush_for_put_image);
            }
            Self::Readback => {
                crate::vk_count!(rpflush_for_readback);
            }
            Self::Present => {
                crate::vk_count!(rpflush_for_present);
            }
            Self::Other => {
                crate::vk_count!(rpflush_for_other);
            }
        }
    }
}

impl RenderEngine {
    // ── Op: render-composite batched path (Stage 5 Task 3) ──────

    /// Try to append the call to an in-flight [`PendingRenderBatch`]
    /// or open a new one. Returns `Ok(Some(stats))` when the call
    /// is batch-eligible AND was successfully appended; the
    /// returned `CompositeStats.deferred_to_batch == true` so the
    /// backend caller suppresses its per-call telemetry / submit-
    /// trace event. Returns `Ok(None)` if the call is NOT
    /// eligible — caller must flush any pending render batch and
    /// fall through to the regular per-call render_composite body.
    ///
    /// Eligibility predicate (conservative, mirrors design):
    /// - `src` and `mask` are `ResolvedSource::Drawable(id)` OR
    ///   `mask == None`. No Solid (would write scratch), no
    ///   Gradient (would carry per-call `axis_projection`).
    /// - `op < 13` (no `dst_readback` path; ops Disjoint/Conjoint
    ///   need a dst snapshot per call which can't share a batch).
    /// - Not self-aliasing: `src.id != dst_id` AND `mask.id != dst_id`.
    /// - Pipeline + descriptor must be identical to the pending
    ///   batch's (encoded into [`RenderBatchKey`] equality).
    ///
    /// # Errors
    ///
    /// `NoVk` on the stub engine; `RendererFailed` on a poisoned
    /// renderer; `Vk` for CB allocation / descriptor / pipeline
    /// failures on the first append.
    #[allow(
        clippy::too_many_arguments,
        reason = "Mirrors render_composite signature"
    )]
    pub(crate) fn try_append_render_batch(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        src: ResolvedSource,
        mask: ResolvedSource,
        dst: Dst,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
        src_repeat: Repeat,
        mask_repeat: Repeat,
        src_transform: Option<PictTransform>,
        mask_transform: Option<PictTransform>,
        mask_component_alpha: bool,
        src_pict_format: u32,
        mask_pict_format: u32,
        dst_pict_format: u32,
    ) -> Result<Option<CompositeStats>, RenderError> {
        let dst_id = dst.id();
        // Self-sample classification (flush-reason telemetry): a
        // composite whose src or mask IS its own dst can never join a
        // same-dst render-pass session — it must flush to read
        // committed pixels. Counted here, before the predicate gates
        // below route it to the non-batched path. Bounds the realistic
        // coalescing win below the consecutive-same-dst ceiling.
        if matches!(src, ResolvedSource::Drawable(sd) if sd.id() == dst_id)
            || matches!(mask, ResolvedSource::Drawable(sd) if sd.id() == dst_id)
        {
            crate::vk_count!(rp_self_sample);
        }

        // Predicate gate 1 — sources.
        let src_sd = match src {
            ResolvedSource::Drawable(sd) if sd.id() != dst_id => sd,
            _ => return Ok(None),
        };
        let src_id = src_sd.id();
        let mask_sd_opt: Option<SourceDrawable> = match mask {
            ResolvedSource::Drawable(sd) if sd.id() != dst_id => Some(sd),
            ResolvedSource::None => None,
            _ => return Ok(None),
        };
        let mask_id_opt: Option<DrawableId> = mask_sd_opt.map(SourceDrawable::id);
        // Predicate gate 2 — op needs no dst readback.
        use crate::kms::vk::render_pipeline::StdPictOp;
        let Some(std_op) = StdPictOp::from_u8(op) else {
            return Ok(None);
        };
        if std_op.needs_dst_readback() {
            return Ok(None);
        }
        // Predicate gate 3 — rects non-empty (else nothing to batch).
        if rects.is_empty() {
            return Ok(None);
        }

        // Key constraint is now minimal — only fields that affect
        // pipeline binding + render-pass attachments. Everything
        // else (src/mask views, transforms, scissors, repeats,
        // pict_formats) is re-encoded per-append.
        let new_key = RenderBatchKey {
            dst: dst_id,
            op,
            dst_pict_format,
            mask_component_alpha,
        };

        // Key-mismatch branch: flush, then re-call to open fresh.
        // Classify same-dst (cross-op merge opportunity) vs diff-dst
        // (genuine pass boundary) for the flush-reason telemetry.
        let key_change_reason = self
            .inner
            .as_ref()
            .and_then(|i| i.pending_render_batch.as_ref())
            .filter(|b| b.key != new_key)
            .map(|b| {
                if b.key.dst == new_key.dst {
                    RenderFlushReason::KeyChangeSameDst
                } else {
                    RenderFlushReason::KeyChangeDiffDst
                }
            });
        if let Some(reason) = key_change_reason {
            self.flush_render_batch(store, platform, reason)?;
        }

        // Lazy-init RENDER assets (mirrors the unbatched path).
        self.ensure_render_assets(platform)?;
        let inner = self.inner.as_mut().ok_or(RenderError::NoVk)?;
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }

        // Resolve dst metadata.
        let (dst_image, dst_view, dst_extent, dst_format, dst_depth) = {
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
            return Ok(None);
        }
        if !matches!(
            dst_format,
            vk::Format::B8G8R8A8_UNORM | vk::Format::R8_UNORM
        ) {
            return Ok(None);
        }
        let dst_has_alpha = dst_has_alpha_for_pict_format(dst_format, dst_depth, dst_pict_format);

        // Resolve src view + extent (drawable_view_cache lookup).
        let src_info =
            drawable_for_render_view(store, src_id).ok_or(RenderError::UnknownDrawable(src_id))?;
        let src_class =
            swizzle_class_for_pict_format(src_info.format, src_info.depth, src_pict_format);
        let src_sampler = sampler_config_for_repeat(src_repeat);
        let src_view = ensure_drawable_view(
            &inner.vk,
            &mut inner.drawable_view_cache,
            src_id,
            src_info.image,
            src_info.format,
            src_sampler,
            src_class,
        )?;
        let src_extent = src_info.extent;

        // Resolve mask view + extent.
        let white_mask_view = inner
            .white_mask_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let (mask_view, mask_extent) = if let Some(mid) = mask_id_opt {
            let info =
                drawable_for_render_view(store, mid).ok_or(RenderError::UnknownDrawable(mid))?;
            let class = swizzle_class_for_pict_format(info.format, info.depth, mask_pict_format);
            let sampler = sampler_config_for_repeat(mask_repeat);
            let view = ensure_drawable_view(
                &inner.vk,
                &mut inner.drawable_view_cache,
                mid,
                info.image,
                info.format,
                sampler,
                class,
            )?;
            (view, info.extent)
        } else {
            (
                white_mask_view,
                vk::Extent2D {
                    width: 1,
                    height: 1,
                },
            )
        };

        // Pipeline lookup.
        let pipeline = inner
            .render_pipelines
            .as_mut()
            .expect("ensured")
            .get(std_op, dst_format, dst_has_alpha, mask_component_alpha)
            .map_err(|e| {
                log::warn!("render try_append_render_batch: pipeline build failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        let pipeline_layout = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .pipeline_layout();

        // Build CompositeAttrs (force_opaque + repeat + transforms).
        let src_force_opaque = resolve_force_opaque_pict_format(store, &src, src_pict_format);
        let mask_force_opaque = resolve_force_opaque_pict_format(store, &mask, mask_pict_format);
        let user_src_xform =
            crate::kms::backend::pixman_transform_to_affine(src_transform.as_ref(), src_extent);
        let user_mask_xform =
            crate::kms::backend::pixman_transform_to_affine(mask_transform.as_ref(), mask_extent);
        let effective_src_repeat = crate::kms::backend::repeat_to_shader_const(src_repeat);
        let effective_mask_repeat = if mask_id_opt.is_some() {
            crate::kms::backend::repeat_to_shader_const(mask_repeat)
        } else {
            crate::kms::vk::render_pipeline::REPEAT_PAD
        };
        let attrs = crate::kms::vk::ops::render::CompositeAttrs {
            src_extent,
            mask_extent,
            // #133 step 3 (P4): the batched path samples the same
            // pictures, so it folds the same content origins in.
            src_offset: [src_sd.offset().0, src_sd.offset().1],
            mask_offset: mask_sd_opt.map_or([0, 0], |sd| [sd.offset().0, sd.offset().1]),
            src_repeat: effective_src_repeat,
            mask_repeat: effective_mask_repeat,
            src_force_opaque,
            mask_force_opaque,
            src_xform: user_src_xform,
            mask_xform: user_mask_xform,
        };

        // Build clip scissor list (same clamping as unbatched path).
        // #133 step 3 (P4): bounds, not the raw extent. Scissors are
        // re-encoded per append, so two windows sharing one backing with
        // different content clips still batch correctly.
        let clip_scissors = build_render_clip_scissors_to(clip_rects, dst.bounds_in(dst_extent));
        if clip_scissors.is_empty() {
            return Ok(Some(CompositeStats {
                deferred_to_batch: true,
                ..CompositeStats::default()
            }));
        }

        // Allocate THIS call's descriptor set (binds this
        // append's src + mask views). With the relaxed predicate,
        // every append gets its own descriptor — pipeline + dst
        // are shared across the batch but the per-draw inputs
        // are not.
        inner.acquire_generation += 1;
        let generation = inner.acquire_generation;
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into_ring(
                &mut inner.descriptor_pool_ring,
                generation,
                src_view,
                mask_view,
                white_mask_view, // dummy dst_readback (no readback in batched path)
            )?;

        // Branch A: open a fresh batch (no pending).
        let is_open = inner.pending_render_batch.is_some();
        if !is_open {
            let (cb, ticket) = begin_op_cb(inner, platform)?;
            // Use an adapter so `record_render_composite_open` can
            // update the dst's tracked layout.
            let mut adapter = {
                let d = store.get_mut(dst_id).expect("checked");
                StorageCompositeTarget {
                    extent: dst_extent,
                    image: dst_image,
                    image_view: dst_view,
                    current_layout: d.storage.current_layout,
                }
            };
            crate::kms::vk::ops::render::record_render_composite_open(
                &inner.vk,
                cb,
                &mut adapter,
                pipeline,
            )?;
            // record_render_composite_open does NOT mutate the
            // tracked layout (that happens at _close). Update the
            // adapter's snapshot back into Drawable.storage now so
            // intermediate observers (none expected in this path)
            // see COLOR_ATTACHMENT_OPTIMAL between open and close.
            {
                let d = store.get_mut(dst_id).expect("checked");
                d.storage.current_layout = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
            }
            // First-append draws (binds this call's descriptor set).
            crate::kms::vk::ops::render::record_render_composite_draws(
                &inner.vk,
                cb,
                pipeline_layout,
                descriptor_set,
                dst_extent,
                &attrs,
                rects,
                &clip_scissors,
            );
            // Accumulate damage.
            let mut dst_damage = Vec::with_capacity(rects.len());
            for cr in rects {
                let rect = vk::Rect2D {
                    offset: vk::Offset2D {
                        x: cr.dst_x,
                        y: cr.dst_y,
                    },
                    extent: vk::Extent2D {
                        width: cr.width,
                        height: cr.height,
                    },
                };
                dst_damage.push(clamp_rect(rect, dst_extent));
            }
            let accumulated_draws =
                u32::try_from(rects.len() * clip_scissors.len()).unwrap_or(u32::MAX);
            let mut touched = HashSet::new();
            touched.insert(src_id);
            if let Some(mid) = mask_id_opt {
                touched.insert(mid);
            }
            // Stage 5 Task 3 fix (UAF on Rembrandt iGPU, 2026-05-22):
            // touch every drawable the batch's CB now references
            // (dst + src + mask) with the batch ticket. Pre-fix
            // this only happened in `flush_render_batch`, leaving
            // a window where an intervening FreePixmap(src) before
            // flush would destroy the VkImage while the batch CB
            // still samples it.
            store.touch_render_fence(dst_id, ticket.clone());
            store.touch_render_fence(src_id, ticket.clone());
            if let Some(mid) = mask_id_opt {
                store.touch_render_fence(mid, ticket.clone());
            }
            inner.pending_render_batch = Some(PendingRenderBatch {
                cb,
                ticket,
                key: new_key,
                dst_damage,
                touched_drawables: touched,
                any_mask: mask_id_opt.is_some(),
                accumulated_draws,
                coalesced_count: 1,
            });
            return Ok(Some(CompositeStats {
                recorded_draws: accumulated_draws,
                deferred_to_batch: true,
                ..CompositeStats::default()
            }));
        }

        // Branch B: append to the existing batch (key matched by
        // the early check; pipeline still bound from open).
        // record_render_composite_draws will bind THIS call's
        // descriptor set inside the open render pass.
        let batch_cb = inner
            .pending_render_batch
            .as_ref()
            .expect("pending batch present in append branch")
            .cb;
        crate::kms::vk::ops::render::record_render_composite_draws(
            &inner.vk,
            batch_cb,
            pipeline_layout,
            descriptor_set,
            dst_extent,
            &attrs,
            rects,
            &clip_scissors,
        );
        // Update batch state.
        let added_draws = u32::try_from(rects.len() * clip_scissors.len()).unwrap_or(u32::MAX);
        let batch = inner
            .pending_render_batch
            .as_mut()
            .expect("pending batch present");
        batch.accumulated_draws = batch.accumulated_draws.saturating_add(added_draws);
        batch.coalesced_count = batch.coalesced_count.saturating_add(1);
        batch.touched_drawables.insert(src_id);
        if let Some(mid) = mask_id_opt {
            batch.touched_drawables.insert(mid);
            batch.any_mask = true;
        }
        // Stage 5 Task 3 fix: touch the new drawables the append
        // just added to `touched_drawables` with the batch ticket
        // (see open branch above for rationale). Dst's ticket is
        // already set from open; appending doesn't change it.
        let batch_ticket = batch.ticket.clone();
        store.touch_render_fence(src_id, batch_ticket.clone());
        if let Some(mid) = mask_id_opt {
            store.touch_render_fence(mid, batch_ticket);
        }
        for cr in rects {
            let rect = vk::Rect2D {
                offset: vk::Offset2D {
                    x: cr.dst_x,
                    y: cr.dst_y,
                },
                extent: vk::Extent2D {
                    width: cr.width,
                    height: cr.height,
                },
            };
            batch.dst_damage.push(clamp_rect(rect, dst_extent));
        }
        Ok(Some(CompositeStats {
            recorded_draws: batch.accumulated_draws,
            deferred_to_batch: true,
            ..CompositeStats::default()
        }))
    }

    /// Flush the pending render batch (if any). Records
    /// `cmd_end_rendering` + exit layout transition, ends + submits
    /// the CB, clones the fence ticket onto every drawable touched
    /// by the batch (dst + src + optional mask), applies
    /// accumulated damage, pushes a `SubmittedOp` + one
    /// `RenderFlushRecord` for backend drain.
    ///
    /// Returns `Some(coalesced_count)` if a batch was flushed,
    /// `None` if there was nothing pending. Caller uses the
    /// count for telemetry (`record_render_batch_flushed`).
    pub(crate) fn flush_render_batch(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        reason: RenderFlushReason,
    ) -> Result<Option<u32>, RenderError> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(None);
        };
        let Some(batch) = inner.pending_render_batch.take() else {
            return Ok(None);
        };
        // A real flush: an open batch was present and is being closed.
        // Attribute it for the `vk renderpass flush src` telemetry.
        reason.record();
        if platform.renderer_failed {
            log::debug!(
                "render flush_render_batch: renderer_failed; dropping batch \
                 (coalesced {} composites)",
                batch.coalesced_count,
            );
            return Ok(None);
        }

        // Resolve dst metadata (image + extent + tracked layout).
        let (dst_image, dst_view, dst_extent) = {
            let d = store
                .get(batch.key.dst)
                .ok_or(RenderError::UnknownDrawable(batch.key.dst))?;
            (d.storage.image, d.storage.image_view, d.storage.extent)
        };

        // Close the render pass + transition dst back to
        // SHADER_READ_ONLY.
        let mut adapter = StorageCompositeTarget {
            extent: dst_extent,
            image: dst_image,
            image_view: dst_view,
            current_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        };
        crate::kms::vk::ops::render::record_render_composite_close(
            &inner.vk,
            batch.cb,
            &mut adapter,
        );
        {
            let d = store.get_mut(batch.key.dst).expect("checked");
            d.storage.current_layout = adapter.current_layout;
        }

        // End + submit (append to group).
        end_and_submit_op(inner, platform, batch.cb, &batch.ticket)?;

        // CPU bookkeeping.
        store.touch_render_fence(batch.key.dst, batch.ticket.clone());
        for tid in &batch.touched_drawables {
            store.touch_render_fence(*tid, batch.ticket.clone());
        }
        for rect in &batch.dst_damage {
            store.damage(batch.key.dst, *rect);
        }
        inner.acquire_generation += 1;
        let generation = inner.acquire_generation;
        let coalesced_count = batch.coalesced_count;
        inner.pending_group_ops.push(SubmittedOp {
            cb: batch.cb,
            ticket: batch.ticket,
            staging: None,
            scratch: Vec::new(),
            sampled_scratch: Vec::new(),
            atlas_ticket: None,
            generation,
            retired_resources: Vec::new(),
        });
        inner.render_flush_records.push(RenderFlushRecord {
            dst: batch.key.dst,
            op: batch.key.op,
            has_mask: batch.any_mask,
            coalesced_count,
        });
        // `inner` borrow released. Auto-flush for render_batch (no semaphore path).
        self.maybe_auto_flush_submit_group(store, platform)?;
        Ok(Some(coalesced_count))
    }

    /// Drain the queue of render-batch flush records. Backend
    /// calls this once per `maybe_composite` tick.
    pub(crate) fn drain_render_flush_records(&mut self) -> Vec<RenderFlushRecord> {
        let Some(inner) = self.inner.as_mut() else {
            return Vec::new();
        };
        std::mem::take(&mut inner.render_flush_records)
    }
}
