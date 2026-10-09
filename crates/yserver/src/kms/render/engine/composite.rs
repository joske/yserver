use super::*;

impl RenderEngineInner {
    /// Phase B.2 Task 4 (USER-codex U-R6.F1 — LOAD-BEARING):
    /// overlay-as-source-of-truth read accessor for the layout of
    /// `id` from the perspective of the next in-frame paint op.
    ///
    /// - When a frame is open: consults the `FrameLayoutTable`. If the
    ///   drawable has been first-touched in-frame, returns its
    ///   `current_in_frame_layout`. Otherwise falls back to
    ///   `Drawable::storage.current_layout` (the pre-frame value).
    /// - When no frame is open: returns `Drawable::storage.current_layout`
    ///   directly (legacy per-op path; storage is the source of truth).
    ///
    /// Storage fallback: a drawable that isn't in `store` resolves to
    /// `UNDEFINED` — matches `Storage::for_tests_null`'s default and
    /// is the only sensible answer for a missing entry; callers that
    /// dereference the result for a barrier source must have already
    /// validated the id.
    ///
    /// Open-frame paint-op ports (Tasks 11-13) MUST use this accessor
    /// to read the dst/src/mask drawable's old_layout when emitting
    /// barriers — see Pitfall 5 in
    /// `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md`.
    /// Reading `storage.current_layout` directly during recording
    /// returns a STALE value (storage is deliberately not mutated
    /// during recording so failed frames roll back via overlay drop).
    #[allow(
        dead_code,
        reason = "B.2 Task 4: helper lands now; Tasks 11+ rewire the open-frame \
                  render_composite path to call this accessor instead of \
                  reading storage directly."
    )]
    pub(crate) fn current_layout_for_drawable(
        &self,
        store: &DrawableStore,
        id: DrawableId,
    ) -> vk::ImageLayout {
        let storage_fallback = store
            .get(id)
            .map(|d| d.storage.current_layout)
            .unwrap_or(vk::ImageLayout::UNDEFINED);
        if let Some(open) = self.frame_builder.open.as_ref() {
            open.layouts
                .current_layout_for_drawable(id, storage_fallback)
        } else {
            storage_fallback
        }
    }
}

impl RenderEngine {
    /// Stage 3c: how many cached drawable views the engine
    /// currently holds. Test-only — used to assert eviction on
    /// drawable retire. Also exposed to integration tests via
    /// `KmsBackend::drawable_view_cache_len` — not gated on
    /// `cfg(test)` because `tests/` integration crates compile
    /// against the regular lib build, not the `--cfg test` one.
    pub(crate) fn drawable_view_cache_len(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |i| i.drawable_view_cache.len())
    }

    /// Stage 3c: whether the lazy-built RENDER pipeline cache has
    /// been constructed. Test-only — used to assert the lazy
    /// build trigger.
    #[cfg(test)]
    pub(crate) fn render_pipelines_built(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.render_pipelines.is_some())
    }

    /// Stage 3c: lazy-initialize RENDER paint assets — pipeline
    /// cache + 1×1 SolidFill / SolidMask / WhiteMask scratches +
    /// `DstReadback`. Idempotent. Called by `render_composite`
    /// and `render_fill_rectangles` on first paint; v1 builds
    /// these eagerly at backend construction, but the v2 engine
    /// is constructed before its first composite request so
    /// paying the cost on first use (typically warmup) is fine.
    ///
    /// The `white_mask_image` requires a one-shot clear-to-white
    /// CB to seed its texel; the recorded clear synchronously
    /// drains via `run_one_shot_op` so the texel is present
    /// before the first sample.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `Vk(...)` for any underlying Vk failure during pipeline
    ///   cache / scratch image / readback construction or the
    ///   one-shot white-clear submit.
    pub(crate) fn ensure_render_assets(
        &mut self,
        platform: &PlatformBackend,
    ) -> Result<(), RenderError> {
        use crate::kms::vk::render_pipeline::record_solid_color_clear;

        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }

        if inner.render_pipelines.is_none() {
            let cache = RenderPipelineCache::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("render ensure_render_assets: RenderPipelineCache::new failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            inner.render_pipelines = Some(cache);
        }

        if inner.masked_blit.is_none() {
            let mb = crate::kms::vk::masked_blit_pipeline::MaskedBlitPipeline::new(Arc::clone(
                &inner.vk,
            ))
            .map_err(|e| {
                log::error!("render ensure_render_assets: MaskedBlitPipeline::new failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            inner.masked_blit = Some(mb);
        }
        // B.2 fix (vkdebug VUID-vkCmdDraw-None-09600): the per-op
        // `record_solid_color_clear` emits a barrier with
        // `old_layout = solid.current_layout()`. Validation tracks
        // layouts across CB boundaries: if the image is still in
        // UNDEFINED globally (never transitioned via any submitted CB),
        // the expectation that the barrier consumes "the layout
        // recorded at descriptor-write time" fails when the descriptor
        // declares SHADER_READ_ONLY. The white-mask path below already
        // seeded its image to SHADER_READ_ONLY via a one-shot clear;
        // mirror that for solid_src/solid_mask. Cost: two extra
        // synchronous submits at engine init.
        let pool_for_init_clears = platform.ops_command_pool_handle().ok_or_else(|| {
            log::error!(
                "render ensure_render_assets: no ops_command_pool for solid-image init clears"
            );
            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
        })?;
        if inner.solid_src_image.is_none() {
            let mut s = SolidColorImage::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!("render ensure_render_assets: solid_src SolidColorImage failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            crate::kms::vk::ops::run_one_shot_op(&inner.vk, pool_for_init_clears, |vk, cb| {
                record_solid_color_clear(vk, cb, &mut s, [0.0, 0.0, 0.0, 0.0]);
                Ok(())
            })
            .map_err(|e| {
                log::error!(
                    "render ensure_render_assets: solid_src init-clear submit failed: {e:?}"
                );
                RenderError::Vk(e)
            })?;
            log::info!(
                "render ensure_render_assets: solid_src_image image={:?} view={:?}",
                s.image(),
                s.image_view(),
            );
            inner.solid_src_image = Some(s);
        }
        if inner.solid_mask_image.is_none() {
            let mut s = SolidColorImage::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!(
                    "render ensure_render_assets: solid_mask SolidColorImage failed: {e:?}"
                );
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            crate::kms::vk::ops::run_one_shot_op(&inner.vk, pool_for_init_clears, |vk, cb| {
                record_solid_color_clear(vk, cb, &mut s, [0.0, 0.0, 0.0, 0.0]);
                Ok(())
            })
            .map_err(|e| {
                log::error!(
                    "render ensure_render_assets: solid_mask init-clear submit failed: {e:?}"
                );
                RenderError::Vk(e)
            })?;
            log::info!(
                "render ensure_render_assets: solid_mask_image image={:?} view={:?}",
                s.image(),
                s.image_view(),
            );
            inner.solid_mask_image = Some(s);
        }
        if inner.white_mask_image.is_none() {
            let mut s = SolidColorImage::new(Arc::clone(&inner.vk)).map_err(|e| {
                log::error!(
                    "render ensure_render_assets: white_mask SolidColorImage failed: {e:?}"
                );
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
            crate::kms::vk::ops::run_one_shot_op(&inner.vk, pool_for_init_clears, |vk, cb| {
                record_solid_color_clear(vk, cb, &mut s, [1.0, 1.0, 1.0, 1.0]);
                Ok(())
            })
            .map_err(|e| {
                log::error!("render ensure_render_assets: white-clear submit failed: {e:?}");
                RenderError::Vk(e)
            })?;
            log::info!(
                "render ensure_render_assets: white_mask_image image={:?} view={:?}",
                s.image(),
                s.image_view(),
            );
            inner.white_mask_image = Some(s);
        }
        if inner.dst_readback.is_none() {
            inner.dst_readback = Some(DstReadback::new(Arc::clone(&inner.vk)));
        }
        if inner.src_alias_readback.is_none() {
            inner.src_alias_readback = Some(DstReadback::new(Arc::clone(&inner.vk)));
        }
        Ok(())
    }

    /// Drop (and `vkDestroyImageView`) every cached drawable view keyed
    /// on `id`. Shared body of [`Self::notify_drawable_retired`] (called
    /// when a drawable's storage is destroyed) and the GLX-TFP promotion
    /// path (called after `Storage::adopt_exportable` swaps the backing
    /// image — the cache keys on `DrawableId` only and never re-checks
    /// the `VkImage` handle, so a swap without invalidation would keep
    /// sampling the OLD image).
    pub(crate) fn invalidate_drawable_views(&mut self, id: DrawableId) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let device = &inner.vk.device;
        inner.drawable_view_cache.retain(|(d, _, _), cached| {
            if *d != id {
                return true;
            }
            unsafe {
                device.destroy_image_view(cached.view, None);
            }
            false
        });
    }

    // ── Op: render_composite (Stage 3c) ─────────────────────────

    /// Record a RENDER `Composite` against `dst`. `src` and `mask`
    /// are pre-resolved by the backend wrapper from the protocol
    /// `PictureRecord`. `rects` are pre-decoded composite quads
    /// in dst coords; `clip_rects` is the dst picture's clip set,
    /// already pre-shifted by the picture's `clip_x` / `clip_y`
    /// origin (Stage 3b's `set_picture_clip_rectangles` site does
    /// the shift). Passing `None` for `clip_rects` paints the
    /// full dst extent; passing an empty slice paints nothing.
    ///
    /// Stage 3c scope (per plan §3c):
    /// - Standard PictOps 0..=12 + Saturate (13) via fixed-function
    ///   blend; Disjoint (16..=27) + Conjoint (32..=43) via the
    ///   shader-side `dst_readback` blend.
    /// - Per-rect picture-clip scissoring — one draw call per
    ///   clip-rect intersection, **NOT** v1's union-bbox shortcut.
    /// - Self-aliasing (`src.drawable_id() == Some(dst_id)`):
    ///   handled via Stage 2d's [`allocate_scratch_image`] —
    ///   copy dst → scratch first, sample scratch_view.
    /// - Component-alpha pass through to the pipeline cache key.
    ///
    /// Deliberate v1 deviations / out-of-scope-for-3c gaps:
    /// - **Gradient sources**: gap log + bail (Stage 3e wires
    ///   gradient LUT build via `picture_paint`).
    /// - **Mask self-alias** (`mask.drawable_id() == Some(dst_id)`):
    ///   gap log + bail. Real apps don't hit this; if rendercheck
    ///   spots a case, fold into 3e alongside the gradient work.
    /// - **No ambient `current_clip` consultation** — RENDER ops
    ///   consult picture clip only (plan §4); the GC's
    ///   `current_clip` lives outside the engine call.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` if `dst_id` is missing from `store`.
    /// - `Vk(...)` for any underlying pipeline / submit failure.
    /// - `RendererFailed` when `platform.renderer_failed`.
    ///
    /// Out-of-scope gating (unknown op, gradient source, mask
    /// self-alias, unsupported dst format) returns `Ok` with
    /// `recorded_draws = 0` — the op silently no-ops, matching
    /// v1's `try_vk_render_composite` shape.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_composite(
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
    ) -> Result<CompositeStats, RenderError> {
        // FrameBuilder-routed unconditionally. The pre-B.2 immediate-
        // submit legacy body and its kill-switch were removed
        // 2026-06-04 along with the main frame-builder gate: the
        // off-paths had bit-rotted and no non-frame-builder path
        // exists anymore. No M2 close here — this IS the frame
        // builder; closing the open frame at the top would defeat op
        // collapse.
        //
        // XFCE-submenu fix — SOURCE-CLASS submit boundary. A Composite
        // whose SOURCE is an active redirect-target backing (xfwm's
        // redirected popup window backings) is submitted in ISOLATION:
        // the open frame is closed before AND after recording it, so no
        // other op shares its submission. When >1 redirect-source
        // Composite batches into one submit, the compositor intermittently
        // samples stale/zero source content and the popup composites empty
        // (submenu "painted into its backing but absent from xfwm's
        // frame"), self-healing on the next incidental recomposite. This
        // only bites when the frame batches ≥2 such composites, which is
        // why it reproduces on integrated GPUs (eiger/air, and any bare-TTY
        // launch that pauses the loop between composites) but not the
        // discrete RX580 or a lightdm launch (both drain ~1 composite per
        // submit). Global close-after-EACH proved the fix but stormed
        // Cinnamon (300-400 submits/s); a dependency-(write→sample) keyed
        // boundary failed to reproduce its correctness. Restricting to the
        // source CLASS keeps ordinary composites (GL compositors, app
        // paints) batched while isolating exactly xfwm's popup composites.
        // HW-confirmed on eiger TTY 2026-07-13.
        let src_is_redirect_backing = match &src {
            ResolvedSource::Drawable(sd) => store.is_active_redirect_target(sd.id()),
            _ => false,
        };
        if src_is_redirect_backing {
            self.close_open_frame(
                store,
                platform,
                crate::kms::render::frame_builder::CloseReason::RedirectSourceBoundary,
            )?;
        }
        let stats = self.render_composite_via_frame_builder(
            store,
            platform,
            op,
            src,
            mask,
            dst,
            rects,
            clip_rects,
            src_repeat,
            mask_repeat,
            src_transform,
            mask_transform,
            mask_component_alpha,
            src_pict_format,
            mask_pict_format,
            dst_pict_format,
        )?;
        if src_is_redirect_backing {
            self.close_open_frame(
                store,
                platform,
                crate::kms::render::frame_builder::CloseReason::RedirectSourceBoundary,
            )?;
        }
        Ok(stats)
    }

    /// Phase B.2 Task 9: frame-builder composite path — prelude only.
    ///
    /// Implements Phase 9A (scratch peek + close-on-grow, NO state
    /// mutation yet) + Phase 9B (open frame + ticket-touch dst).
    /// Subsequent tasks (10-13) fill in src/mask resolution, scratch
    /// pinning, descriptor acquisition, op record, and emit.
    ///
    /// **Phase 9A — close-then-grow ordering is LOAD-BEARING** (USER-
    /// codex U-R10.F1). The grow must happen BEFORE any new frame
    /// opens. With no open frame at the time of `ensure_returning_old`,
    /// the engine's `adopt_retired_resource_for_gpu_retirement`
    /// helper falls through case (a) (open frame) and attaches the
    /// retired Box to `submitted.back` — the just-closed frame's
    /// `SubmittedOp` — so its `release(&vk)` rides the in-flight CB's
    /// fence rather than the about-to-open new frame's pin set.
    ///
    /// The dispatcher deliberately does NOT call the M2 close before
    /// invoking this: under sub-gate=ON this method IS the frame
    /// builder, so the open frame must remain open across consecutive
    /// composites.
    ///
    /// # Errors
    ///
    /// Same shape as [`Self::render_composite`].
    #[allow(clippy::too_many_arguments)]
    fn render_composite_via_frame_builder(
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
    ) -> Result<CompositeStats, RenderError> {
        use crate::kms::vk::{ops::render as vk_render, render_pipeline::StdPictOp};

        let dst_id = dst.id();
        let stats = CompositeStats::default();
        if rects.is_empty() {
            return Ok(stats);
        }

        // (0) Flush pre-existing cow/render batches so they submit
        //     under their own (per-op) ticket before this call opens a
        //     frame.
        self.flush_render_batch(store, platform, RenderFlushReason::Other)?;

        // (1) Lazy-init RENDER assets (pipelines, solid 1x1 images,
        //     scratch slots).
        self.ensure_render_assets(platform)?;

        // (2) PHASE 9A — scratch peek + close-on-grow. NO state
        //     mutation yet (beyond the assets ensure above which is
        //     idempotent + doesn't touch the open frame).
        //
        //     Resolve dst metadata. Scoped so the `&self.inner` borrow
        //     is released before any later `as_mut()` re-borrow.
        let (dst_image, dst_view, dst_extent, dst_format, dst_depth) = {
            let _inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
            if platform.renderer_failed {
                return Err(RenderError::RendererFailed);
            }
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
            log::debug!(
                "render render_composite (frame_builder) gap: dst format \
                 {dst_format:?} not BGRA/R8 (dst id={dst_id:?})"
            );
            return Ok(stats);
        }
        let dst_has_alpha = dst_has_alpha_for_pict_format(dst_format, dst_depth, dst_pict_format);

        // Map the protocol op byte to the pipeline cache's enum.
        let Some(std_op) = StdPictOp::from_u8(op) else {
            log::debug!(
                "render render_composite (frame_builder) gap: unsupported op {op} \
                 (dst id={dst_id:?})"
            );
            return Ok(stats);
        };
        let needs_dst_readback = std_op.needs_dst_readback();
        let src_self_alias = matches!(src, ResolvedSource::Drawable(sd) if sd.id() == dst_id);
        let mask_self_alias = matches!(mask, ResolvedSource::Drawable(sd) if sd.id() == dst_id);
        let self_alias_used = src_self_alias || mask_self_alias;

        // (2a) PEEK growth. Both scratches (when needed) grow to
        //      (dst_format, dst_extent.width, dst_extent.height). If
        //      the slot is empty (`None`), `fits` defaults to false
        //      → grow.
        let need_grow_dst_rb = needs_dst_readback && {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .dst_readback
                .as_ref()
                .map(|rb| !rb.fits(dst_format, dst_extent.width, dst_extent.height))
                .unwrap_or(true)
        };
        let need_grow_alias = self_alias_used && {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .src_alias_readback
                .as_ref()
                .map(|rb| !rb.fits(dst_format, dst_extent.width, dst_extent.height))
                .unwrap_or(true)
        };

        // (2b) If growth would fire AND a frame is open with prior ops,
        //      close BEFORE touching anything for the current op.
        //      Pitfall 4 — guards record_copy_from at emit-time from
        //      writing into a scratch instance newer than the one the
        //      recorded views resolved against.
        if (need_grow_dst_rb || need_grow_alias) && {
            let inner = self.inner.as_ref().expect("inner");
            inner
                .frame_builder
                .open
                .as_ref()
                .is_some_and(|o| o.has_recorded_work())
        } {
            self.close_open_frame(
                store,
                platform,
                crate::kms::render::frame_builder::CloseReason::ScratchGrow,
            )?;
        }

        // (2c) CRITICAL: grow + adopt BEFORE opening the new frame
        //      (USER-codex U-R10.F1). If we grew AFTER opening, the
        //      helper's case (a) would attach the retired Box to the
        //      NEW frame's pin set — a new-frame abort would then
        //      release Vk handles while the just-closed CB is still
        //      sampling them. With no open frame here, the helper
        //      falls through to case (b) and rides `submitted.back`'s
        //      fence (the just-closed frame's SubmittedOp).
        if need_grow_dst_rb {
            let retired = {
                let inner = self.inner.as_mut().expect("inner");
                inner
                    .dst_readback
                    .as_mut()
                    .expect("ensured")
                    .ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                    .map_err(|e| {
                        log::warn!(
                            "render render_composite (frame_builder): dst_readback \
                             ensure failed: {e:?}"
                        );
                        RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                    })?
            };
            let inner = self.inner.as_mut().expect("inner");
            inner.adopt_retired_resource_for_gpu_retirement(retired);
        }
        if need_grow_alias {
            let retired = {
                let inner = self.inner.as_mut().expect("inner");
                inner
                    .src_alias_readback
                    .as_mut()
                    .expect("ensured")
                    .ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
                    .map_err(|e| {
                        log::warn!(
                            "render render_composite (frame_builder): \
                             src_alias_readback ensure failed: {e:?}"
                        );
                        RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
                    })?
            };
            let inner = self.inner.as_mut().expect("inner");
            inner.adopt_retired_resource_for_gpu_retirement(retired);
        }

        // (3) PHASE 9B — open + ticket-touch dst. Scratch slots are
        //     now sized correctly; Task 10's view queries don't grow.
        //
        //     Phase B.2 Mechanism 2: bump `acquire_generation` once at
        //     open and capture the resulting value on the OpenFrame.
        let inner = self.inner.as_mut().expect("inner");
        if !inner.frame_builder.is_open() {
            // Release the inner borrow before calling the platform
            // method which doesn't need it.
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");

        // (4) Ticket-touch dst + snapshot prior ticket + FIRST-TOUCH
        //     dst layout overlay (the overlay's pre_frame_layout is
        //     what `rollback_pre_submit` writes back on close-failure).
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();
        let prior_dst_ticket = store.get(dst_id).and_then(|d| d.last_render_ticket.clone());
        let dst_pre_frame_layout = inner.current_layout_for_drawable(store, dst_id);
        {
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.touched.first_touch(dst_id, prior_dst_ticket);
            open.layouts
                .first_touch_drawable(dst_id, dst_pre_frame_layout);
        }
        store.touch_render_fence(dst_id, frame_ticket.clone());

        // (5) Resolve solid scratch views directly — these are 1×1
        //     engine-owned `SolidColorImage`s that never grow, so no
        //     pin / no ticket-touch is needed (Pitfall 4b). Engine
        //     `Drop` destroys them at shutdown after all frames have
        //     closed.
        let inner = self.inner.as_ref().expect("inner");
        let solid_src_view = inner
            .solid_src_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let solid_mask_view = inner
            .solid_mask_image
            .as_ref()
            .expect("ensured")
            .image_view();
        let white_mask_view = inner
            .white_mask_image
            .as_ref()
            .expect("ensured")
            .image_view();

        // (5b) Self-alias readback view (src or mask == dst). Phase
        //      9A already grew the scratch slot if needed; here we
        //      just query the view. `view()` takes `&mut self`
        //      because it may lazily build the no-alpha variant on
        //      first `dst_has_alpha=false` call against this scratch
        //      instance, so we re-borrow `inner` mutably.
        let src_alias_view = if self_alias_used {
            let inner = self.inner.as_ref().expect("inner");
            debug_assert!(
                inner.src_alias_readback.as_ref().is_some_and(|rb| rb.fits(
                    dst_format,
                    dst_extent.width,
                    dst_extent.height,
                )),
                "Phase 9A failed to grow src_alias_readback to required size",
            );
            let inner = self.inner.as_mut().expect("inner");
            match inner
                .src_alias_readback
                .as_mut()
                .expect("ensured")
                .view(dst_format, dst_has_alpha)
            {
                Ok(Some(v)) => Some(v),
                Ok(None) => {
                    log::warn!(
                        "render render_composite (frame_builder): \
                         src_alias_readback view None — skipping"
                    );
                    return Ok(stats);
                }
                Err(e) => {
                    log::warn!(
                        "render render_composite (frame_builder): \
                         src_alias_readback view build failed: {e:?}"
                    );
                    return Ok(stats);
                }
            }
        } else {
            None
        };

        // (6) dst_readback view when the op needs the shader-side
        //     blend (Disjoint/Conjoint). Phase 9A already grew if
        //     needed; same `&mut self` re-borrow as src_alias above.
        let dst_readback_view = if needs_dst_readback {
            let inner = self.inner.as_ref().expect("inner");
            debug_assert!(
                inner.dst_readback.as_ref().is_some_and(|rb| rb.fits(
                    dst_format,
                    dst_extent.width,
                    dst_extent.height,
                )),
                "Phase 9A failed to grow dst_readback to required size",
            );
            let inner = self.inner.as_mut().expect("inner");
            match inner
                .dst_readback
                .as_mut()
                .expect("ensured")
                .view(dst_format, dst_has_alpha)
            {
                Ok(Some(v)) => Some(v),
                Ok(None) => {
                    log::warn!(
                        "render render_composite (frame_builder): \
                         dst_readback view None — skipping"
                    );
                    return Ok(stats);
                }
                Err(e) => {
                    log::warn!(
                        "render render_composite (frame_builder): \
                         dst_readback view build failed: {e:?}"
                    );
                    return Ok(stats);
                }
            }
        } else {
            None
        };

        // (7) Resolve src view + extent + (optional) clear colour.
        //     Mirrors `render_composite_legacy` (Drawable / Solid /
        //     Gradient / None branches), with the addition of:
        //
        //     - per-Drawable `store.touch_render_fence` (frame-wide
        //       ticket pin),
        //     - per-Drawable `open.touched.first_touch` + layout
        //       `first_touch_drawable` snapshot for close-failure
        //       rollback.
        //
        //     dst was first-touched in step (4) above; we skip the
        //     touch when src/mask resolves to dst (self-alias case
        //     — the descriptor binding rides `src_alias_view`
        //     resolved in step 5b instead of the drawable view
        //     cache, so the cache lookup is skipped too).
        //
        //     Gradient sources resolve through `inner.picture_paint`
        //     which is engine-owned and CPU-immutable for the
        //     picture's lifetime (codex R3 finding 9). No ticket-
        //     touch / pin: the engine holds the LUT past frame
        //     close, and `picture_paint_remove` cannot run mid-paint.
        let mut src_clear_color: Option<[f32; 4]> = None;
        let mut mask_clear_color: Option<[f32; 4]> = None;
        let mut src_is_synthetic_1x1 = false;
        let mut mask_is_synthetic_1x1 = false;
        let mut src_picture_xform: Option<vk_render::AffineXform> = None;
        let mut mask_picture_xform: Option<vk_render::AffineXform> = None;

        // #133 step 3 (P4): the third element is the SAMPLING OFFSET —
        // the picture drawable's content origin inside the sampled
        // storage. `(0, 0)` for every synthetic source and for every
        // pixmap / `bw == 0` window, so this is the pre-#133 value
        // everywhere it was correct before.
        let (src_view, src_extent, src_sample_offset) = if src_self_alias {
            // Self-alias: bind the alias scratch instead of dst's
            // drawable view. dst was already first-touched in
            // step (4); no additional touch here.
            (
                src_alias_view.expect("set when self_alias_used"),
                dst_extent,
                (0, 0),
            )
        } else {
            match src {
                ResolvedSource::Drawable(sd) => {
                    let id = sd.id();
                    // Snapshot prior + layout BEFORE first_touch so we
                    // capture the pre-frame state.
                    let prior = store.get(id).and_then(|d| d.last_render_ticket.clone());
                    let pre_layout = {
                        let inner = self.inner.as_ref().expect("inner");
                        inner.current_layout_for_drawable(store, id)
                    };
                    {
                        let inner = self.inner.as_mut().expect("inner");
                        let open = inner.frame_builder.open.as_mut().expect("just opened");
                        open.touched.first_touch(id, prior);
                        open.layouts.first_touch_drawable(id, pre_layout);
                    }
                    store.touch_render_fence(id, frame_ticket.clone());

                    let info = drawable_for_render_view(store, id)
                        .ok_or(RenderError::UnknownDrawable(id))?;
                    // Audit #4: pict_format-aware swizzle so an
                    // xRGB32 source on a depth-32 storage picks the
                    // BgraNoAlpha (force α=ONE) sample view.
                    let class =
                        swizzle_class_for_pict_format(info.format, info.depth, src_pict_format);
                    let sampler = sampler_config_for_repeat(src_repeat);
                    let inner = self.inner.as_mut().expect("inner");
                    let view = ensure_drawable_view(
                        &inner.vk,
                        &mut inner.drawable_view_cache,
                        id,
                        info.image,
                        info.format,
                        sampler,
                        class,
                    )?;
                    (view, info.extent, sd.offset())
                }
                ResolvedSource::Solid(color) => {
                    src_clear_color = Some(color);
                    src_is_synthetic_1x1 = true;
                    (
                        solid_src_view,
                        vk::Extent2D {
                            width: 1,
                            height: 1,
                        },
                        (0, 0),
                    )
                }
                ResolvedSource::Gradient(xid) => {
                    let inner = self.inner.as_ref().expect("inner");
                    match inner.picture_paint.get(&xid) {
                        Some(PicturePaintState::Gradient(g)) => {
                            src_picture_xform = Some(g.axis_projection());
                            (g.image_view(), g.extent(), (0, 0))
                        }
                        None => {
                            log::debug!(
                                "render render_composite (frame_builder) gap: \
                                 gradient picture 0x{xid:x} missing from \
                                 engine.picture_paint (LUT build likely failed)"
                            );
                            return Ok(stats);
                        }
                    }
                }
                ResolvedSource::None => {
                    log::debug!(
                        "render render_composite (frame_builder) gap: src is \
                         None (protocol requires src)"
                    );
                    return Ok(stats);
                }
            }
        };

        // (8) Resolve mask view + extent + sampling offset. Same shape
        // as src — a MASK picture on a bordered window has the same
        // content origin problem as a source (#133 step 3 (P4)).
        let (mask_view, mask_extent, mask_sample_offset) = if mask_self_alias {
            (
                src_alias_view.expect("set when self_alias_used"),
                dst_extent,
                (0, 0),
            )
        } else {
            match mask {
                ResolvedSource::Drawable(sd) => {
                    let id = sd.id();
                    let prior = store.get(id).and_then(|d| d.last_render_ticket.clone());
                    let pre_layout = {
                        let inner = self.inner.as_ref().expect("inner");
                        inner.current_layout_for_drawable(store, id)
                    };
                    {
                        let inner = self.inner.as_mut().expect("inner");
                        let open = inner.frame_builder.open.as_mut().expect("just opened");
                        open.touched.first_touch(id, prior);
                        open.layouts.first_touch_drawable(id, pre_layout);
                    }
                    store.touch_render_fence(id, frame_ticket.clone());

                    let info = drawable_for_render_view(store, id)
                        .ok_or(RenderError::UnknownDrawable(id))?;
                    // Audit #4: same pict_format-aware swizzle as src.
                    let class =
                        swizzle_class_for_pict_format(info.format, info.depth, mask_pict_format);
                    let sampler = sampler_config_for_repeat(mask_repeat);
                    let inner = self.inner.as_mut().expect("inner");
                    let view = ensure_drawable_view(
                        &inner.vk,
                        &mut inner.drawable_view_cache,
                        id,
                        info.image,
                        info.format,
                        sampler,
                        class,
                    )?;
                    (view, info.extent, sd.offset())
                }
                ResolvedSource::Solid(color) => {
                    mask_clear_color = Some(color);
                    mask_is_synthetic_1x1 = true;
                    (
                        solid_mask_view,
                        vk::Extent2D {
                            width: 1,
                            height: 1,
                        },
                        (0, 0),
                    )
                }
                ResolvedSource::Gradient(xid) => {
                    let inner = self.inner.as_ref().expect("inner");
                    match inner.picture_paint.get(&xid) {
                        Some(PicturePaintState::Gradient(g)) => {
                            mask_picture_xform = Some(g.axis_projection());
                            (g.image_view(), g.extent(), (0, 0))
                        }
                        None => {
                            log::debug!(
                                "render render_composite (frame_builder) gap: \
                                 gradient mask picture 0x{xid:x} missing \
                                 from engine.picture_paint (LUT build likely \
                                 failed)"
                            );
                            return Ok(stats);
                        }
                    }
                }
                ResolvedSource::None => {
                    mask_is_synthetic_1x1 = true;
                    (
                        white_mask_view,
                        vk::Extent2D {
                            width: 1,
                            height: 1,
                        },
                        (0, 0),
                    )
                }
            }
        };

        // (9) PHASE B.2 Task 11 Step 1: pipeline lookup + descriptor
        //     acquisition. The pipeline cache `get` takes `&mut self`
        //     (builds on cache-miss); release that borrow BEFORE
        //     reaching for `allocate_descriptor_for_views_into_ring`
        //     so the descriptor-pool-ring sibling borrow doesn't
        //     alias.
        let inner = self.inner.as_mut().expect("inner");
        let _pipeline_handle = inner
            .render_pipelines
            .as_mut()
            .expect("ensured")
            .get(std_op, dst_format, dst_has_alpha, mask_component_alpha)
            .map_err(|e| {
                log::warn!(
                    "render render_composite (frame_builder): pipeline build failed \
                     for op {op}: {e:?}"
                );
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        // Mechanism 2: every descriptor acquisition during the open
        // frame uses the captured `frame_generation`. Read it via the
        // OpenFrame (set at open time, not re-bumped per op).
        let frame_generation = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .frame_generation;
        let src_for_descriptor = src_alias_view.unwrap_or(src_view);
        let mask_for_descriptor = mask_view;
        // Pitfall (Task 12 audit): when `!needs_dst_readback`, binding 2
        // (`dst_tex`) is bound but never sampled (the Disjoint/Conjoint
        // shader path is the only consumer). Match legacy's
        // `dst_readback_view.unwrap_or(white_mask_view)` shape here —
        // `white_mask_view` is engine-owned, sized 1×1, and always in
        // `SHADER_READ_ONLY_OPTIMAL` (transitioned once at backend
        // init), so it satisfies the descriptor write's declared image
        // layout. Earlier drafts used `dst_view` which is in
        // `COLOR_ATTACHMENT_OPTIMAL` between open / close — a latent
        // VUID-Vkpipeline-image-layout-mismatch waiting for validation
        // layers to trip on it.
        let dst_for_descriptor = dst_readback_view.unwrap_or(white_mask_view);
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into_ring(
                &mut inner.descriptor_pool_ring,
                frame_generation,
                src_for_descriptor,
                mask_for_descriptor,
                dst_for_descriptor,
            )
            .map_err(RenderError::Vk)?;

        // (10) Step 2: resolve dst_old_layout via the overlay
        //      accessor. Pitfall 5 — for the 2nd op-in-frame, the
        //      overlay reflects op 1's post-op layout
        //      (SHADER_READ_ONLY_OPTIMAL); reading
        //      `store.get(dst_id).storage.current_layout` directly
        //      would return the STALE pre-frame value because
        //      storage is intentionally not mutated during recording.
        let inner = self.inner.as_ref().expect("inner");
        let dst_old_layout = inner.current_layout_for_drawable(store, dst_id);

        // (11) Step 3: build the replay-ready CompositeAttrs via the
        //      shared helper extracted from `_legacy`. The payload
        //      records this verbatim; close-time replay feeds it to
        //      `record_render_composite_draws` unchanged.
        let attrs = build_render_composite_attrs(
            store,
            &src,
            &mask,
            src_pict_format,
            mask_pict_format,
            src_extent,
            mask_extent,
            src_sample_offset,
            mask_sample_offset,
            src_repeat,
            mask_repeat,
            src_is_synthetic_1x1,
            mask_is_synthetic_1x1,
            src_picture_xform,
            mask_picture_xform,
            src_transform.as_ref(),
            mask_transform.as_ref(),
        );

        // (12) Step 4: append RecordedOp::RenderComposite via the
        //      atomicity helper. Pitfall 6 / codex round 4 finding 3 —
        //      `push_op_and_set_layouts` is the ONLY path that mutates
        //      ops + overlay in tandem. The overlay update is ONE write
        //      per op, to the POST-op layout the recorder's close-
        //      transition will leave dst at (SHADER_READ_ONLY_OPTIMAL).
        //      No intermediate COLOR_ATTACHMENT_OPTIMAL write — that's
        //      an in-CB transient never observable across ops.
        let recorded = crate::kms::render::frame_builder::RecordedRenderComposite {
            op,
            dst_id,
            dst_image,
            dst_view,
            dst_extent,
            dst_format,
            dst_has_alpha,
            dst_old_layout,
            src_view,
            mask_view,
            src_alias_view,
            dst_readback_view,
            attrs,
            src_clear_color,
            mask_clear_color,
            mask_component_alpha,
            needs_dst_readback,
            rects: rects.to_vec().into_boxed_slice(),
            clip_rects: clip_rects.map(|r| r.to_vec().into_boxed_slice()),
            // #133 step 3 (P4): the content clip must survive into the
            // DEFERRED emit, which is where this op's scissors are built.
            // `None` = the whole storage (bw == 0).
            dst_bounds: dst.bounds(),
            descriptor_set,
        };
        {
            let inner = self.inner.as_mut().expect("inner");
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.push_op_and_set_layouts(
                crate::kms::render::frame_builder::RecordedOp::RenderComposite(Box::new(recorded)),
                &[(dst_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }
        store.mark_contents_modified(dst_id);

        // (13) Step 5: damage bookkeeping + recorded-draws stat.
        //      Damage is committed AT APPEND TIME (matches `_legacy`
        //      shape) so subsequent damage queries from non-paint
        //      paths see the union eagerly; close-on-failure rolls
        //      damage back via the layout-overlay-rollback that
        //      `close_open_frame` performs on the touched set.
        let mut stats = stats;
        stats.recorded_draws = u32::try_from(rects.len()).unwrap_or(u32::MAX);
        for cr in rects {
            #[allow(clippy::cast_possible_wrap)]
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
            store.damage(dst_id, clamp_rect_to(rect, dst.bounds_in(dst_extent)));
        }

        Ok(stats)
    }

    // ── Op: render_fill_rectangles (Stage 3c) ───────────────────

    /// X RENDER `FillRectangles`: paint `rects` with a single
    /// premultiplied colour using PictOp `op`. Per plan §3c
    /// "Scope", this is `render_composite(op, SolidFill(color),
    /// NoMask, dst, ...)` — one composite with N rects.
    ///
    /// # Errors
    ///
    /// Same shape as [`render_composite`].
    pub(crate) fn render_fill_rectangles(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        op: u8,
        color: [f32; 4],
        dst: Dst,
        rects: &[crate::kms::vk::ops::render::CompositeRect],
        clip_rects: Option<&[Rectangle16]>,
    ) -> Result<CompositeStats, RenderError> {
        // Phase B Invariant M2: no wrapper-level
        // `close_open_frame_for_non_ported_op` here — this wrapper
        // delegates to `render_composite`, which IS the frame builder
        // (must NOT close). A wrapper-level close here would defeat
        // the collapse of two `render_fill_rectangles` calls into one
        // frame.
        self.render_composite(
            store,
            platform,
            op,
            ResolvedSource::Solid(color),
            ResolvedSource::None,
            dst,
            rects,
            clip_rects,
            Repeat::Pad,
            Repeat::Pad,
            None,
            None,
            false,
            // Audit #4: Solid src has no Picture context — depth
            // heuristic fallback is fine, force-opaque is false
            // anyway for non-Drawable sources.
            0,
            0,
            0,
        )
    }
}

impl SourceDrawable {
    /// The whole storage, sampled from its origin: pixmaps (including
    /// COMPOSITE-named window pixmaps, whose border is part of the
    /// image) and every server-internal source.
    pub(crate) fn whole(id: DrawableId) -> Self {
        Self {
            id,
            offset: (0, 0),
            domain: None,
        }
    }

    /// A drawable sampled as a window's CONTENT: `offset` is the
    /// content origin inside the sampled storage, `domain` the
    /// window's own extent.
    pub(crate) fn content(id: DrawableId, offset: (i32, i32), domain: vk::Extent2D) -> Self {
        Self {
            id,
            offset,
            domain: Some(domain),
        }
    }

    pub(crate) fn id(self) -> DrawableId {
        self.id
    }

    /// Texel offset added to the sampling origin, so `xSrc = 0` lands
    /// on the picture drawable's own first pixel.
    pub(crate) fn offset(self) -> (i32, i32) {
        self.offset
    }

    /// The logical source extent; `None` = the whole storage.
    ///
    /// Consumed by `KmsBackend::picture_source_domain_clip`, which
    /// turns it into the dst-space `RepeatNone` domain clip for
    /// `Composite` (Xorg's `miClipPictureSrc` shape,
    /// `render/mipict.c:353-356`). The sampling OFFSET above is applied
    /// for every op family that samples a picture — `Composite` (both
    /// the batched and unbatched paths and both deferred emits) and the
    /// `Trapezoids`/`Triangles` composite stage — because they all fold
    /// it in through `CompositeAttrs::src_offset` in one recorder.
    ///
    /// Known residue, all bounded to reading the SOURCE WINDOW'S OWN
    /// ring and none of it reachable at `bw == 0`:
    /// - `Trapezoids`/`Triangles` get the offset but no domain clip.
    ///   Their scissors are built at append time while the source
    ///   origin is finalised at emit, and that path does not fold the
    ///   source's CLIENT clip either — a pre-existing gap this does not
    ///   widen.
    /// - A transformed source has no rectangular domain in dst space
    ///   (Xorg leaves that to sample-time checking too).
    /// - `Normal`/`Pad`/`Reflect` wrap or clamp against the sampled
    ///   image rather than suppressing, so they can still reach the
    ///   ring. Xorg's fb path is looser still: its pixman image is the
    ///   whole containing pixmap (`fb/fbpict.c:293-296`), so an
    ///   unredirected window repeats over the entire screen pixmap.
    pub(crate) fn domain(self) -> Option<vk::Extent2D> {
        self.domain
    }
}

/// X11 Render PictFormat force-opaque resolver.
///
/// Per the X11 Render spec, a Picture whose format has
/// `alpha_mask == 0` (e.g. a depth-24 RGB visual: r8g8b8 or
/// x8r8g8b8) must yield samples with `α = 1.0` regardless of the
/// byte content of the underlying storage. v2 stores depth-24
/// pixmaps as `B8G8R8A8_UNORM` with the α byte as server-owned
/// padding — without this override, marco/compositing samples the
/// padding byte (often 0) and the operator collapses to no-op,
/// leaving widget windows invisible under a compositing WM.
///
/// ## Gate: `depth == 24` BGRA storage only
///
/// Stage 4d landed this as `depth == 24` rather than the broader
/// `depth < 32` because depth-8 (A8 alpha-only pictures) and
/// depth-1 (bitmap masks) carry meaningful α in the X11 Render
/// `PictFormat` — forcing `α = 1.0` on a depth-8 mask would
/// silently turn coverage masks into solid blocks. Picture-
/// format-driven resolution (looking up the actual `PictFormat`
/// attached to the source picture rather than the drawable
/// depth) is the cleaner long-term shape; `depth == 24` is the
/// load-bearing case marco-with-compositing depends on, so we
/// fix that first and broaden later if a non-depth-24 picture
/// format with `alpha_mask == 0` shows up in a real workload.
///
/// `Solid` carries its own α, `Gradient` LUTs are authored with
/// the right α from `RenderCreateGradient`, and `None` is the
/// synthetic white-mask path — none of those need the override.
pub(super) fn resolve_force_opaque(store: &DrawableStore, src: &ResolvedSource) -> bool {
    match src {
        ResolvedSource::Drawable(sd) => store.get(sd.id()).is_some_and(|d| d.depth == 24),
        ResolvedSource::Solid(_) | ResolvedSource::Gradient(_) | ResolvedSource::None => false,
    }
}

/// Audit #4 (2026-05-19) — pict_format-aware force-opaque decision.
/// A picture with PictFormat declaring `alpha_mask = 0` (xRGB24 or
/// xRGB32) says "the storage's α byte is padding, not client-
/// meaningful." Engine must force α=1 regardless of storage depth.
///
/// `pict_format == 0` falls back to the legacy depth heuristic for
/// engine-internal callers that synthesize sources without a real
/// Picture (composite_glyphs/trapezoids backfills). Both helpers
/// coexist: the pict_format-aware path threads through the
/// `render_composite` call site; the older `resolve_force_opaque`
/// stays for the synthesized-source paths.
pub(super) fn resolve_force_opaque_pict_format(
    store: &DrawableStore,
    src: &ResolvedSource,
    pict_format: u32,
) -> bool {
    use yserver_protocol::x11::{RENDER_FMT_RGB24, RENDER_FMT_XRGB32};
    match src {
        ResolvedSource::Drawable(sd) => {
            if pict_format == RENDER_FMT_RGB24 || pict_format == RENDER_FMT_XRGB32 {
                return true;
            }
            store.get(sd.id()).is_some_and(|d| d.depth == 24)
        }
        ResolvedSource::Solid(_) | ResolvedSource::Gradient(_) | ResolvedSource::None => false,
    }
}

/// Phase B.2 Task 11 (USER-codex U-R11.F1+F2 / U-R12.F2): shared
/// `CompositeAttrs` builder lifted out of `render_composite_legacy` so
/// `render_composite_via_frame_builder` records an attrs payload that
/// reproduces the legacy pre-call construction byte-for-byte. The
/// recorded payload is replayed at close-time by
/// `record_render_composite_draws` (via `record_render_composite_open_with_old_layout`
/// in B.2 Task 12) so any divergence here would alter pixel output
/// relative to the pre-frame-builder path.
///
/// Inputs mirror the per-call locals the legacy body has already
/// resolved (synthetic-1x1 flags, gradient picture transforms, user
/// pict-transforms, pict-format-aware force-opaque flags). The helper
/// does NOT pack repeat / force-opaque into `RenderPushConsts`-style
/// bits — `record_render_composite_draws` handles that at emit time.
#[allow(clippy::too_many_arguments)]
fn build_render_composite_attrs(
    store: &DrawableStore,
    src: &ResolvedSource,
    mask: &ResolvedSource,
    src_pict_format: u32,
    mask_pict_format: u32,
    src_extent: vk::Extent2D,
    mask_extent: vk::Extent2D,
    // #133 step 3 (P4) — each picture drawable's content origin inside
    // the storage being sampled; `(0, 0)` unless the picture wraps a
    // bordered window.
    src_sample_offset: (i32, i32),
    mask_sample_offset: (i32, i32),
    src_repeat: Repeat,
    mask_repeat: Repeat,
    src_is_synthetic_1x1: bool,
    mask_is_synthetic_1x1: bool,
    src_picture_xform: Option<crate::kms::vk::ops::render::AffineXform>,
    mask_picture_xform: Option<crate::kms::vk::ops::render::AffineXform>,
    src_transform: Option<&PictTransform>,
    mask_transform: Option<&PictTransform>,
) -> crate::kms::vk::ops::render::CompositeAttrs {
    // Synthetic 1×1 scratches use PAD so the single texel covers the
    // whole rect. Otherwise pass the bare shader repeat constant.
    let effective_src_repeat = if src_is_synthetic_1x1 {
        crate::kms::vk::render_pipeline::REPEAT_PAD
    } else {
        crate::kms::backend::repeat_to_shader_const(src_repeat)
    };
    let effective_mask_repeat = if mask_is_synthetic_1x1 {
        crate::kms::vk::render_pipeline::REPEAT_PAD
    } else {
        crate::kms::backend::repeat_to_shader_const(mask_repeat)
    };

    // Compose gradient picture's intrinsic xform with the user's
    // RenderSetPictureTransform — matches v1's `compose_affines(
    // intrinsic, user)` shape.
    let user_src_xform = crate::kms::backend::pixman_transform_to_affine(src_transform, src_extent);
    let user_mask_xform =
        crate::kms::backend::pixman_transform_to_affine(mask_transform, mask_extent);
    let combined_src_xform = match src_picture_xform {
        Some(intrinsic) => crate::kms::backend::compose_affines(intrinsic, user_src_xform),
        None => user_src_xform,
    };
    let combined_mask_xform = match mask_picture_xform {
        Some(intrinsic) => crate::kms::backend::compose_affines(intrinsic, user_mask_xform),
        None => user_mask_xform,
    };

    let src_force_opaque = resolve_force_opaque_pict_format(store, src, src_pict_format);
    let mask_force_opaque = resolve_force_opaque_pict_format(store, mask, mask_pict_format);

    crate::kms::vk::ops::render::CompositeAttrs {
        src_extent,
        mask_extent,
        src_offset: [src_sample_offset.0, src_sample_offset.1],
        mask_offset: [mask_sample_offset.0, mask_sample_offset.1],
        src_repeat: effective_src_repeat,
        mask_repeat: effective_mask_repeat,
        src_force_opaque,
        mask_force_opaque,
        src_xform: combined_src_xform,
        mask_xform: combined_mask_xform,
    }
}

pub(super) fn drawable_for_render_view(
    store: &DrawableStore,
    id: DrawableId,
) -> Option<DrawableViewInfo> {
    let d = store.get(id)?;
    Some(DrawableViewInfo {
        image: d.storage.image,
        extent: d.storage.extent,
        format: d.storage.format,
        depth: d.depth,
    })
}

pub(super) fn sampler_config_for_repeat(r: Repeat) -> SamplerConfig {
    match r {
        Repeat::None => SamplerConfig::Clamp,
        Repeat::Normal => SamplerConfig::Repeat,
        Repeat::Pad => SamplerConfig::Pad,
        Repeat::Reflect => SamplerConfig::Reflect,
    }
}

/// Map the pre-resolved shader repeat constant (see
/// `crate::kms::backend::repeat_to_shader_const`) back to the matching
/// `SamplerConfig`. The deferred trap/tri emit stores the repeat as the
/// shader constant in its payload, so the src view's Vk sampler must be
/// derived from it — mirroring how every non-deferred composite path
/// derives the sampler from the picture's `Repeat`. Defaults to `Clamp`
/// (REPEAT_NONE) for any unrecognised value.
pub(super) fn sampler_config_for_shader_repeat(c: u32) -> SamplerConfig {
    use crate::kms::vk::render_pipeline::{REPEAT_NORMAL, REPEAT_PAD, REPEAT_REFLECT};
    if c == REPEAT_NORMAL as u32 {
        SamplerConfig::Repeat
    } else if c == REPEAT_PAD as u32 {
        SamplerConfig::Pad
    } else if c == REPEAT_REFLECT as u32 {
        SamplerConfig::Reflect
    } else {
        SamplerConfig::Clamp
    }
}

fn swizzle_class_for(format: vk::Format, depth: u8) -> SwizzleClass {
    match (format, depth) {
        (vk::Format::R8_UNORM, _) => SwizzleClass::AlphaOnlyR8,
        (vk::Format::B8G8R8A8_UNORM, 24) => SwizzleClass::BgraNoAlpha,
        _ => SwizzleClass::RgbaIdent,
    }
}

/// Audit #4 (2026-05-19) — pict_format-aware destination
/// `has_alpha` decision. A Picture wrapping a depth-32 storage
/// with `RENDER_FMT_XRGB32` declares `alpha_mask = 0` — the dst
/// storage's α byte is padding, NOT a client-meaningful alpha
/// channel. The pipeline + readback selection must treat it as
/// "no alpha target" (same as depth-24), else post-composite reads
/// of the padding bytes leak through to subsequent samples as
/// partial transparency.
///
/// Pre-fix `dst_has_alpha = dst_depth == 32` unconditionally. Now
/// the picture's PictFormat takes precedence over storage depth
/// when known (xRGB24 / xRGB32 → no alpha; ARGB32 → has alpha);
/// `pict_format == 0` falls back to the depth+format heuristic
/// for engine-internal callers without picture context.
pub(super) fn dst_has_alpha_for_pict_format(
    format: vk::Format,
    depth: u8,
    pict_format: u32,
) -> bool {
    use yserver_protocol::x11::{RENDER_FMT_ARGB32, RENDER_FMT_RGB24, RENDER_FMT_XRGB32};
    // R8_UNORM dst is an A8 mask — alpha-only by definition,
    // pict_format can't override that.
    if format == vk::Format::R8_UNORM {
        return true;
    }
    if pict_format == RENDER_FMT_RGB24 || pict_format == RENDER_FMT_XRGB32 {
        return false;
    }
    if pict_format == RENDER_FMT_ARGB32 {
        return true;
    }
    // Fallback: legacy depth heuristic.
    depth == 32
}

/// Audit #4 (2026-05-19) — pict_format-aware swizzle. A picture
/// with PictFormat declaring `alpha_mask = 0` (xRGB24 or xRGB32)
/// must bind a sample view whose α swizzle pins to ONE, regardless
/// of storage depth. Pre-fix the engine cached one view per
/// (drawable, sampler, swizzle) tuple where swizzle came from
/// storage-depth alone — so a depth-32 storage wrapped by an
/// xRGB32 picture got `RgbaIdent` (pass-through), and the storage's
/// α padding bytes (typically 0) leaked into the composite as
/// transparent.
///
/// `pict_format == 0` falls back to `swizzle_class_for` for
/// internal engine paths that don't carry a Picture identity
/// (composite_glyphs synthesized A8 masks, trapezoid traps).
pub(super) fn swizzle_class_for_pict_format(
    format: vk::Format,
    depth: u8,
    pict_format: u32,
) -> SwizzleClass {
    use yserver_protocol::x11::{RENDER_FMT_RGB24, RENDER_FMT_XRGB32};
    // R8_UNORM is alpha-only by construction — pict_format can't
    // override that. Same for the legacy depth-24 BGRA8 case.
    if format == vk::Format::R8_UNORM {
        return SwizzleClass::AlphaOnlyR8;
    }
    if format == vk::Format::B8G8R8A8_UNORM {
        if pict_format == RENDER_FMT_RGB24 || pict_format == RENDER_FMT_XRGB32 {
            return SwizzleClass::BgraNoAlpha;
        }
        if depth == 24 {
            return SwizzleClass::BgraNoAlpha;
        }
    }
    SwizzleClass::RgbaIdent
}

/// Lookup/build a `vk::ImageView` for `id` with the given
/// (sampler, swizzle) classification. The cache key splits on
/// SamplerConfig so a Repeat=None vs Repeat=Pad sample of the
/// same drawable doesn't share — Stage 3c uses Nearest only, so
/// sampler is "address mode" rather than full sampler state.
/// Address mode actually lives in the pipeline cache's sampler
/// (one shared linear sampler) — the cache split is therefore
/// over-engineered for 3c but matches the plan's published
/// (DrawableId, SamplerConfig, SwizzleClass) key, leaving room
/// for Stage 5's per-address-mode sampler splits without a
/// cache-shape rewrite.
pub(super) fn ensure_drawable_view(
    vk: &VkContext,
    cache: &mut HashMap<(DrawableId, SamplerConfig, SwizzleClass), CachedDrawableView>,
    id: DrawableId,
    image: vk::Image,
    format: vk::Format,
    sampler: SamplerConfig,
    class: SwizzleClass,
) -> Result<vk::ImageView, vk::Result> {
    let key = (id, sampler, class);
    if let Some(c) = cache.get(&key) {
        return Ok(c.view);
    }
    let components = match class {
        SwizzleClass::RgbaIdent => vk::ComponentMapping {
            r: vk::ComponentSwizzle::IDENTITY,
            g: vk::ComponentSwizzle::IDENTITY,
            b: vk::ComponentSwizzle::IDENTITY,
            a: vk::ComponentSwizzle::IDENTITY,
        },
        SwizzleClass::AlphaOnlyR8 => vk::ComponentMapping {
            r: vk::ComponentSwizzle::ZERO,
            g: vk::ComponentSwizzle::ZERO,
            b: vk::ComponentSwizzle::ZERO,
            a: vk::ComponentSwizzle::R,
        },
        SwizzleClass::BgraNoAlpha => vk::ComponentMapping {
            r: vk::ComponentSwizzle::IDENTITY,
            g: vk::ComponentSwizzle::IDENTITY,
            b: vk::ComponentSwizzle::IDENTITY,
            a: vk::ComponentSwizzle::ONE,
        },
    };
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .components(components)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        );
    let view = unsafe { vk.device.create_image_view(&info, None)? };
    cache.insert(key, CachedDrawableView { view });
    Ok(view)
}

impl CompositeTarget for StorageCompositeTarget {
    fn vk_image(&self) -> vk::Image {
        self.image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.image_view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        self.current_layout
    }
    fn set_current_layout(&mut self, layout: vk::ImageLayout) {
        self.current_layout = layout;
    }
}
