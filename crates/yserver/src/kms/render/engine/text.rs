use super::*;

impl RenderEngineInner {
    /// Look up or lazily build the text pipeline for
    /// `(op, dst_format, dst_has_alpha, component_alpha)`. Mirrors
    /// the RENDER `Composite` pipeline cache's get-or-build
    /// (`render_pipeline.rs`) — including its fourth key dimension,
    /// so the two caches key on the same four facts; blend state
    /// comes from `StdPictOp::blend_factors` so the two paths agree
    /// by construction. Callers must have validated `op` as a
    /// standard fixed-function PictOp (0..=12, not Saturate) and
    /// built `glyph_atlas` first (the pipeline's descriptor set
    /// binds the atlas view at construction).
    ///
    /// `component_alpha` must already be gated on the device's
    /// `dualSrcBlend` — `RenderEngine::effective_glyph_layout` is the
    /// one place that decides it, so a `true` here means the atlas
    /// really does hold four planes for this run.
    ///
    /// Build happens at RECORD time (where `&mut self` is
    /// available); emit only looks the entry up — a recorded
    /// `CompositeGlyphs`/`ImageText` op's pipeline is guaranteed
    /// present by this call.
    pub(super) fn ensure_text_pipeline(
        &mut self,
        op: u8,
        dst_format: vk::Format,
        dst_has_alpha: bool,
        component_alpha: bool,
        context: &str,
    ) -> Result<(), RenderError> {
        use crate::kms::vk::render_pipeline::StdPictOp;
        if let std::collections::hash_map::Entry::Vacant(e) =
            self.text_pipelines
                .entry((op, dst_format, dst_has_alpha, component_alpha))
        {
            let Some(std_op) = StdPictOp::from_u8(op) else {
                // Callers gate to the standard family before this.
                log::error!("render {context}: ensure_text_pipeline got invalid op {op}");
                return Err(RenderError::Vk(vk::Result::ERROR_UNKNOWN));
            };
            let atlas_view = self
                .glyph_atlas
                .as_ref()
                .ok_or(RenderError::NoVk)?
                .image_view();
            match TextPipeline::new(
                Arc::clone(&self.vk),
                dst_format,
                std_op,
                dst_has_alpha,
                component_alpha,
                atlas_view,
            ) {
                Ok(p) => {
                    e.insert(p);
                }
                Err(err) => {
                    log::error!("render {context}: TextPipeline::new failed: {err:?}");
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }
        Ok(())
    }
}

impl RenderEngine {
    // ── Op: image_text (Stage 3a) ───────────────────────────────

    /// One glyph the caller hands to [`RenderEngine::image_text`].
    /// CPU-side pre-rasterised by FreeType so the engine doesn't
    /// touch FreeType state. `pixels` is row-major, tightly packed
    /// (no row padding) — width × height alpha bytes.
    ///
    /// The pen-left/pen-top offsets are applied to `dst_x` /
    /// `dst_y` by the caller, so the engine just packs the glyph
    /// and queues a draw at the supplied destination coords.
    /// Stage 3a: drive a single text run against `target`'s
    /// storage. CPU-side glyph rasterisation is the caller's
    /// concern (KmsBackend wraps the v1 FreeType path); the
    /// engine takes the resulting [`PreparedGlyph`] slice, interns
    /// each into the atlas, and records one TextPipeline draw
    /// covering the whole run.
    ///
    /// `font_xid` keys the glyph cache so the same codepoint
    /// rendered at two different font sizes ends up at two atlas
    /// slots. `foreground_rgba` is the GC foreground in [0..1].
    /// Damage is recorded on the target at the union of glyph
    /// bounding boxes.
    ///
    /// Returns telemetry counts the caller feeds to the v2 backend
    /// telemetry sink: how many distinct atlas interns happened
    /// (= miss count this run), how many glyph uploads were
    /// submitted (= same as interns today; collapses if later
    /// coalesced), and how many glyphs were dropped due to
    /// atlas-full.
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` when `target` isn't in `store`.
    /// - `Vk(...)` for any CB / submit failure. Best-effort: an
    ///   upload that fails partway is logged and the affected
    ///   glyph is dropped; only catastrophic failures (text-run
    ///   CB allocation, atlas init) propagate.
    pub(crate) fn image_text(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst: Dst,
        font_xid: u32,
        foreground_rgba: [f32; 4],
        rendered: &[PreparedGlyph],
    ) -> Result<ImageTextStats, RenderError> {
        // Phase B.3 Task 14: image_text body rewrite — frame-builder path.
        // Body order per N9: empty-input → renderer_failed → flush_render_batch
        // → preflight (format gate N7 LOAD-BEARING) → lazy-init → open frame
        // → first_touch + atlas snapshot → glyph loop → push ImageText.

        let target = dst.id();
        // (0) Empty-input fast-path.
        let mut stats = ImageTextStats::default();
        if rendered.is_empty() {
            return Ok(stats);
        }

        // (1) renderer_failed check.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }

        // (2) Flush pre-existing render batch before opening the frame.
        // NO flush_cow_batch — that helper is deleted in Task 4.
        self.flush_render_batch(store, platform, RenderFlushReason::Glyph)?;

        // (3) Preflight: store lookup + TARGET-FORMAT GATE (N7 LOAD-BEARING).
        // Gate fires BEFORE any atlas first-touch / glyph upload / op append.
        // No rollback path needed because nothing has been recorded yet.
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let (target_extent, target_format) = {
            let d = store
                .get(target)
                .ok_or(RenderError::UnknownDrawable(target))?;
            (d.storage.extent, d.storage.format)
        };
        if target_format != vk::Format::B8G8R8A8_UNORM {
            log::warn!(
                "render image_text (frame_builder): target xid={:?} has format {:?}; \
                 text pipeline only supports B8G8R8A8_UNORM — dropping run",
                store.get(target).map(|d| d.xid),
                target_format,
            );
            return Ok(stats);
        }

        // (4) Lazy-init GlyphAtlas + TextPipeline (preserve
        //     engine.rs:4531-4553 verbatim in spirit).
        if inner.glyph_atlas.is_none() {
            match GlyphAtlas::new(Arc::clone(&inner.vk)) {
                Ok(a) => inner.glyph_atlas = Some(a),
                Err(e) => {
                    log::error!("render image_text (frame_builder): GlyphAtlas::new failed: {e:?}");
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }
        // (4a) Atlas room, before the frame opens: when this run's
        //      misses no longer fit, close the open frame and empty
        //      the atlas; every glyph in use re-uploads on its next draw.
        let fits = inner.glyph_atlas_misses_fit(rendered.iter().map(|g| {
            (
                GlyphKey {
                    font_xid,
                    codepoint: g.codepoint,
                },
                u32::try_from(g.w).unwrap_or(u32::MAX),
                u32::try_from(g.h).unwrap_or(u32::MAX),
            )
        }));
        if !fits {
            self.reset_glyph_atlas(store, platform)?;
        }
        let inner = self.inner.as_mut().expect("inner");
        // Core ImageText is always the Over+BGRA8 blend — the
        // legacy singleton entry, bit-identical blend state.
        //
        // And never component-alpha: core text bitmaps are A8 by
        // construction (the server rasterised them itself from a core
        // font), so `false` is unconditional here. That is one half
        // of design invariant 8; the other half is the per-glyph
        // assertion in the walk below, which is what would catch an
        // atlas entry arriving from the glyphset namespace.
        inner.ensure_text_pipeline(
            3, // Over
            vk::Format::B8G8R8A8_UNORM,
            true,
            false, // component_alpha — core text is single-plane A8
            "image_text (frame_builder)",
        )?;

        // (5) Open frame if not already open.
        if !inner.frame_builder.is_open() {
            let _ = inner;
            let ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner.frame_builder.open_for_paint(ticket, frame_generation);
        }
        let inner = self.inner.as_mut().expect("inner");

        // (6) first_touch dst + ticket-touch dst + first_touch_drawable.
        let frame_ticket = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();
        let prior_dst_ticket = store.get(target).and_then(|d| d.last_render_ticket.clone());
        let dst_pre_frame_layout = inner.current_layout_for_drawable(store, target);
        {
            let open = inner.frame_builder.open.as_mut().expect("just opened");
            open.touched.first_touch(target, prior_dst_ticket);
            open.layouts
                .first_touch_drawable(target, dst_pre_frame_layout);
        }
        store.touch_render_fence(target, frame_ticket.clone());

        // (7) N7 atlas transactional discipline (LOAD-BEARING): snapshot
        //     atlas_prev_ticket + atlas layout on the FIRST atlas-touching op
        //     in this frame (mirror composite_glyphs_via_frame_builder
        //     engine.rs:5644-5663).
        {
            let atlas_pre_ticket: Option<FenceTicket> = inner
                .glyph_atlas
                .as_ref()
                .and_then(|a| a.last_render_ticket().cloned());
            let atlas_pre_layout: vk::ImageLayout = inner
                .glyph_atlas
                .as_ref()
                .map(crate::kms::render::glyph_atlas::GlyphAtlas::current_layout)
                .unwrap_or(vk::ImageLayout::UNDEFINED);
            let open = inner.frame_builder.open.as_mut().expect("open");
            if open.atlas_prev_ticket_snapshot.is_none() {
                open.atlas_prev_ticket_snapshot = Some(atlas_pre_ticket);
                open.layouts.first_touch_atlas(atlas_pre_layout);
            }
        }

        // (8) Per-glyph walk: lookup → on miss, pack + upload the pixels
        //     into the frame's upload arena (upload_to_frame pins) + push
        //     RecordedOp::GlyphUpload (NOT push_op_and_set_layouts because
        //     GlyphUpload.dst_id() is None — no layout updates).
        let ceiling = inner.frame_builder.max_pinned_resources_per_frame();
        let pending_pins_before_call = inner
            .frame_builder
            .open
            .as_ref()
            .map(|o| o.pins.len())
            .unwrap_or(0);

        let mut glyphs_to_draw: Vec<crate::kms::render::frame_builder::RecordedTextGlyph> =
            Vec::with_capacity(rendered.len());
        let mut new_uploads: Vec<(
            GlyphKey,
            AtlasEntry,
            crate::kms::render::frame_builder::PinnedUploadIdx,
        )> = Vec::new();
        let mut new_zero_inserts: Vec<(GlyphKey, AtlasEntry)> = Vec::new();
        let mut damage_min_x = i32::MAX;
        let mut damage_min_y = i32::MAX;
        let mut damage_max_x = i32::MIN;
        let mut damage_max_y = i32::MIN;

        for g in rendered {
            let key = GlyphKey {
                font_xid,
                codepoint: g.codepoint,
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let w_u = g.w as u32;
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let h_u = g.h as u32;

            // (a) Committed atlas hit?
            let committed_hit = inner.glyph_atlas.as_ref().expect("init").lookup(key);
            // (b) Pending-insert hit in the open frame?
            let pending_hit = inner.frame_builder.open.as_ref().and_then(|o| {
                o.pending_glyph_inserts
                    .entries
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, e)| *e)
            });
            // (c) New-uploads dedupe (same call earlier)?
            let dedupe_hit = new_uploads
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, e, _)| *e);

            let entry = if let Some(hit) = committed_hit.or(pending_hit).or(dedupe_hit) {
                hit
            } else {
                // Zero-area glyph (space): cache degenerate entry; no
                // atlas slot consumed.
                if w_u == 0 || h_u == 0 {
                    let e = AtlasEntry {
                        atlas_x: 0,
                        atlas_y: 0,
                        packed_w: 0,
                        logical_w: 0,
                        h: 0,
                        pen_left: 0,
                        pen_top: 0,
                        layout: GlyphLayout::A8,
                    };
                    new_zero_inserts.push((key, e));
                    continue;
                }
                // Pin-ceiling enforcement: check BEFORE pack() so dropped
                // glyphs don't leak atlas slots (mirror B.1 pattern). The
                // trailing `+ 1` reserves the instance buffer's pin,
                // pinned unconditionally after this loop: when uploads
                // and the draw buffer cannot both fit, drop uploads to
                // leave room for the draw — losing a glyph loses one
                // glyph, losing the draw loses the whole run.
                if new_uploads.len() + 1 + pending_pins_before_call + 1 > ceiling {
                    stats.glyphs_dropped += 1;
                    continue;
                }
                // Pre-validate pixels length BEFORE pack().
                let copy_len = (w_u as usize) * (h_u as usize);
                if g.pixels.len() < copy_len {
                    log::warn!(
                        "render image_text (frame_builder): glyph pixels {} < {} expected; \
                         dropping pre-pack",
                        g.pixels.len(),
                        copy_len,
                    );
                    stats.glyphs_dropped += 1;
                    continue;
                }
                let Some((atlas_x, atlas_y)) =
                    inner.glyph_atlas.as_mut().expect("init").pack(w_u, h_u)
                else {
                    inner
                        .glyph_atlas
                        .as_mut()
                        .expect("init")
                        .note_dropped(w_u, h_u);
                    stats.glyphs_dropped += 1;
                    continue;
                };
                stats.atlas_interns += 1;
                // Pinned into the open frame here; the GlyphUpload op that
                // replays it is recorded below, in the same frame.
                let upload_pin = inner.upload_to_frame(
                    &g.pixels[..copy_len],
                    inner.upload_copy_align,
                    crate::kms::vk::mem_accounting::ChurnClass::GlyphUpload,
                )?;
                let new_entry = AtlasEntry {
                    atlas_x,
                    atlas_y,
                    packed_w: w_u,
                    logical_w: w_u,
                    h: h_u,
                    pen_left: 0,
                    pen_top: 0,
                    layout: GlyphLayout::A8,
                };
                new_uploads.push((key, new_entry, upload_pin));
                stats.glyph_uploads += 1;
                new_entry
            };

            if entry.logical_w == 0 || entry.h == 0 {
                continue;
            }
            // Design invariant 8, asserted rather than assumed:
            // `image_text` and `composite_glyphs` share this atlas
            // and this recorded op, and `GlyphKey.font_xid` carries a
            // CORE FONT host xid here but a GLYPHSET host xid there.
            // A single xid allocator means the two namespaces cannot
            // collide, so a hit here is always one of our own
            // single-plane entries — but were they ever to collide,
            // core text would sample a 4-plane entry as if it had one
            // plane, and nothing else in the path would notice.
            debug_assert_eq!(
                entry.layout,
                GlyphLayout::A8,
                "image_text found a non-A8 atlas entry for key {key:?}; the core-font \
                 and glyphset xid namespaces must not overlap",
            );
            // Project glyph bbox into damage tracker.
            damage_min_x = damage_min_x.min(g.dst_x);
            damage_min_y = damage_min_y.min(g.dst_y);
            #[allow(clippy::cast_possible_wrap)]
            let max_x = g.dst_x.saturating_add(entry.logical_w as i32);
            #[allow(clippy::cast_possible_wrap)]
            let max_y = g.dst_y.saturating_add(entry.h as i32);
            damage_max_x = damage_max_x.max(max_x);
            damage_max_y = damage_max_y.max(max_y);
            glyphs_to_draw.push(crate::kms::render::frame_builder::RecordedTextGlyph {
                atlas_x: entry.atlas_x,
                atlas_y: entry.atlas_y,
                logical_w: entry.logical_w,
                h: entry.h,
                dst_x: g.dst_x,
                dst_y: g.dst_y,
                layout: entry.layout,
            });
        }

        // Commit new uploads + zero-inserts into the open frame.
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            for (key, entry, upload_pin) in new_uploads.drain(..) {
                open.ops
                    .push(crate::kms::render::frame_builder::RecordedOp::GlyphUpload(
                        crate::kms::render::frame_builder::RecordedGlyphUpload {
                            upload_pin,
                            atlas_x: entry.atlas_x,
                            atlas_y: entry.atlas_y,
                            packed_w: entry.packed_w,
                            h: entry.h,
                            insert_key: key,
                            insert_entry: entry,
                        },
                    ));
                open.pending_glyph_inserts.push(key, entry);
                open.glyph_uploads_in_frame = open.glyph_uploads_in_frame.saturating_add(1);
            }
            for (key, entry) in new_zero_inserts.drain(..) {
                open.pending_glyph_inserts.push(key, entry);
            }
        }

        // (9) If glyphs_to_draw is empty after processing → return Ok(stats).
        if glyphs_to_draw.is_empty() {
            return Ok(stats);
        }

        // (10) Damage: union of glyph dst-bboxes (append-time mutation, same
        //      as composite_glyphs_via_frame_builder). Frame failure does NOT
        //      roll back damage — the DamageNotify was already sent.
        if damage_max_x > damage_min_x && damage_max_y > damage_min_y {
            let dx = damage_min_x.max(0);
            let dy = damage_min_y.max(0);
            let w = u32::try_from(damage_max_x - dx).unwrap_or(0);
            let h = u32::try_from(damage_max_y - dy).unwrap_or(0);
            if w > 0 && h > 0 {
                store.damage(
                    target,
                    clamp_rect_to(
                        vk::Rect2D {
                            offset: vk::Offset2D { x: dx, y: dy },
                            extent: vk::Extent2D {
                                width: w,
                                height: h,
                            },
                        },
                        dst.bounds_in(target_extent),
                    ),
                );
            }
        }

        // (11) Build + pin the per-glyph instance vertex buffer (#1
        //      glyph batching), then append RecordedOp::ImageText via
        //      push_op_and_set_layouts with (target, SHADER_READ_ONLY_OPTIMAL).
        let inner = self.inner.as_mut().expect("inner");
        let mut instance_data: Vec<u8> = Vec::with_capacity(
            glyphs_to_draw.len()
                * std::mem::size_of::<crate::kms::vk::text_pipeline::GlyphInstanceData>(),
        );
        for g in &glyphs_to_draw {
            if let Some(inst) = crate::kms::vk::text_pipeline::GlyphInstanceData::from_glyph(
                g.dst_x,
                g.dst_y,
                g.atlas_x,
                g.atlas_y,
                g.logical_w,
                g.h,
                g.layout,
            ) {
                instance_data.extend_from_slice(inst.as_bytes());
            }
        }
        let instance_count = u32::try_from(
            instance_data.len()
                / std::mem::size_of::<crate::kms::vk::text_pipeline::GlyphInstanceData>(),
        )
        .unwrap_or(0);
        if instance_count == 0 {
            return Ok(stats);
        }
        let instance_pin = inner.upload_to_frame(
            &instance_data,
            UPLOAD_VERTEX_ALIGN,
            crate::kms::vk::mem_accounting::ChurnClass::ImageText,
        )?;
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                crate::kms::render::frame_builder::RecordedOp::ImageText(Box::new(
                    crate::kms::render::frame_builder::RecordedImageText {
                        dst_id: target,
                        dst_extent: target_extent,
                        // #133 step 3 (P4): core text carries no picture
                        // clip, so the content bound IS the only scissor.
                        // `None` keeps the unscissored `record_text_run`
                        // path the bw == 0 case has always used.
                        dst_bounds: dst.bounds(),
                        dst_old_layout: dst_pre_frame_layout,
                        foreground_rgba,
                        instance_pin,
                        instance_count,
                    },
                )),
                &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }
        store.mark_contents_modified(target);

        Ok(stats)
    }
}

impl TextRunTarget for StorageTextTarget {
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

// Stage 3c: `record_render_composite` takes the same minimal
// paint-target surface as `record_text_run`. Impl `CompositeTarget`
// on the same adapter so v2's RENDER paint sites can hand the
// recorder a borrow over a `Drawable`'s storage fields.
impl CompositeTarget for StorageTextTarget {
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
