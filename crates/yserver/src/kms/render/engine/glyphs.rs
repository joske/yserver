use super::*;

impl RenderEngineInner {
    /// Whether the glyph atlas can take every glyph of a request that
    /// is not resident yet — not committed, not pending in the open
    /// frame — each key once, at its packed `(w, h)`, in request order.
    /// A glyph too large for even an empty atlas is left out: it drops
    /// either way, and counting it would empty the atlas on every draw.
    pub(super) fn glyph_atlas_misses_fit(
        &self,
        glyphs: impl Iterator<Item = (GlyphKey, u32, u32)>,
    ) -> bool {
        let Some(atlas) = self.glyph_atlas.as_ref() else {
            return true;
        };
        let mut misses: Vec<(GlyphKey, u32, u32)> = glyphs
            .filter(|&(key, w, h)| {
                w != 0 && h != 0 && atlas.fits_empty(w, h) && atlas.lookup(key).is_none()
            })
            .collect();
        if misses.is_empty() {
            return true;
        }
        if let Some(open) = self.frame_builder.open.as_ref() {
            let pending: HashSet<GlyphKey> = open
                .pending_glyph_inserts
                .entries
                .iter()
                .map(|(k, _)| *k)
                .collect();
            misses.retain(|(k, _, _)| !pending.contains(k));
        }
        let mut seen = HashSet::new();
        let sizes: Vec<(u32, u32)> = misses
            .into_iter()
            .filter(|(k, _, _)| seen.insert(*k))
            .map(|(_, w, h)| (w, h))
            .collect();
        atlas.fits(&sizes)
    }
}

impl RenderEngine {
    /// Close the open frame (its pending glyph inserts commit against
    /// the current atlas layout) and empty the glyph atlas.
    ///
    /// Safe against in-flight work: every recorded or submitted draw
    /// that samples an old slot runs before the re-upload that reuses
    /// it, which is ordered behind those samplers by its own
    /// `ALL_COMMANDS → COPY` barrier on the same queue.
    pub(super) fn reset_glyph_atlas(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        self.close_open_frame(
            store,
            platform,
            crate::kms::render::frame_builder::CloseReason::GlyphAtlasFull,
        )?;
        if let Some(atlas) = self.inner.as_mut().and_then(|i| i.glyph_atlas.as_mut()) {
            atlas.reset();
        }
        Ok(())
    }

    /// Forget the atlas entries of glyphset / core font `font_xid`:
    /// all of them (`None`: FreeGlyphSet, CloseFont) or just
    /// `glyph_ids` (FreeGlyphs, or AddGlyphs redefining a live id).
    /// Also drops matching inserts still pending in the open frame, so
    /// the old image cannot be committed and served for a reused id.
    pub(crate) fn forget_glyphs(&mut self, font_xid: u32, glyph_ids: Option<&[u32]>) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        let ids: Option<HashSet<u32>> = glyph_ids.map(|ids| ids.iter().copied().collect());
        let doomed = |k: &GlyphKey| {
            k.font_xid == font_xid && ids.as_ref().is_none_or(|ids| ids.contains(&k.codepoint))
        };
        if let Some(atlas) = inner.glyph_atlas.as_mut() {
            match &ids {
                None => {
                    atlas.forget_font(font_xid);
                }
                Some(ids) => {
                    for &codepoint in ids {
                        atlas.forget(GlyphKey {
                            font_xid,
                            codepoint,
                        });
                    }
                }
            }
        }
        if let Some(open) = inner.frame_builder.open.as_mut() {
            open.pending_glyph_inserts
                .entries
                .retain(|(k, _)| !doomed(k));
        }
    }

    // ── Op: composite_glyphs (Stage 3d) ─────────────────────────

    /// Record a RENDER `CompositeGlyphs` against `dst`. Backend
    /// wrapper (`KmsBackend::render_composite_glyphs`) is
    /// responsible for: (a) gating on `op == Over` + SolidFill
    /// source (plan §3d "v1-parity scope"), (b) parsing the
    /// `items` glyph-element stream including the inline `0xFF 0
    /// mask_fmt new_gs` glyphset-change form, (c) looking up
    /// each glyph from `KmsCore.glyphsets`, (d) host-side A1→A8
    /// expansion. By the time we reach the engine, each input is a
    /// dense A8 bitmap + a dst position + a glyphset xid that
    /// keys it in the engine's atlas.
    ///
    /// `foreground_rgba` is the SolidFill source's premultiplied
    /// colour (the text pipeline shader multiplies it by the
    /// sampled atlas alpha — same blend state as 3a's image_text).
    ///
    /// `clip_rects` is the dst picture's clip set, already
    /// pre-shifted by the picture's `clip_x` / `clip_y` origin
    /// (Stage 3b). `None` paints the full dst; passing an empty
    /// slice paints nothing. Per plan §4, the engine emits one
    /// `cmd_set_scissor` + glyph-draw batch per clip rect — this
    /// is the v1-bug-fix: v1's `try_vk_render_composite_glyphs`
    /// reads the dst picture clip but ignores it
    /// (`kms::backend.rs:5313`).
    ///
    /// # Errors
    ///
    /// - `NoVk` on the stub engine.
    /// - `UnknownDrawable` if `dst_id` is missing.
    /// - `Vk(...)` for any CB / submit failure. Atlas-upload
    ///   failures drop the affected glyph and bump
    ///   `stats.glyphs_dropped`; only catastrophic failures (CB
    ///   alloc, draw-record) propagate.
    /// - `RendererFailed` if `platform.renderer_failed`.
    pub(crate) fn composite_glyphs(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst: Dst,
        op: u8,
        dst_pict_format: u32,
        foreground_rgba: [f32; 4],
        glyphs: &[CompositeGlyphInput<'_>],
        clip_rects: Option<&[Rectangle16]>,
    ) -> Result<ImageTextStats, RenderError> {
        // FrameBuilder-routed unconditionally. The pre-B.1 per-op-
        // submit legacy body and its kill-switch were removed
        // 2026-06-04: the off-path had bit-rotted (close_open_frame
        // asserted at startup) and there is no non-frame-builder path
        // anymore.
        self.composite_glyphs_via_frame_builder(
            store,
            platform,
            dst,
            op,
            dst_pict_format,
            foreground_rgba,
            glyphs,
            clip_rects,
        )
    }

    /// Phase B.1 Task 15: FrameBuilder-routed composite_glyphs.
    /// Defers per-glyph upload submits + the final draw submit into a
    /// single open frame; the frame closes via M2/M3/timeout/sync_wait/
    /// shutdown and submits all recorded ops as ONE `vkQueueSubmit2`.
    ///
    /// Codex-round walkthroughs preserved here:
    /// - R1 finding 2: flush cow/render batches FIRST so any
    ///   pre-existing batch CBs land chronologically before the
    ///   frame's draws.
    /// - R1 finding 3: snapshot dst pre_frame_layout in the
    ///   `FrameLayoutTable` overlay so rollback_pre_submit can write
    ///   it back on close failure.
    /// - R3 finding 2: count UNIQUE prospective misses in a pre-pass
    ///   to avoid premature close+reopen on a call with repeated
    ///   uncached keys.
    /// - R3 finding 2a: after close+reopen, recompute
    ///   pending_pins_before_call (pins reset to zero on reopen).
    /// - R4: pin-ceiling per-glyph check BEFORE `pack()` so dropped
    ///   glyphs don't leak shelf slots.
    /// - R5: pre-validate pixel length BEFORE `pack()` so malformed
    ///   input doesn't leak a slot either.
    /// - Damage mutation at append time — spec § "Damage accumulation"
    ///   mandates it (the client's request was already accepted).
    fn composite_glyphs_via_frame_builder(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst: Dst,
        op: u8,
        dst_pict_format: u32,
        foreground_rgba: [f32; 4],
        glyphs: &[CompositeGlyphInput<'_>],
        clip_rects: Option<&[Rectangle16]>,
    ) -> Result<ImageTextStats, RenderError> {
        let dst_id = dst.id();
        let mut stats = ImageTextStats::default();
        if glyphs.is_empty() {
            return Ok(stats);
        }

        // (0) Flush pre-existing cow/render batches before opening the
        //     frame. Codex R1 finding 2: a pre-opened cow batch's CBs
        //     must submit BEFORE the frame's draws (chronological X11
        //     order). With M2 wired on every non-ported paint op,
        //     batches normally close before a frame opens — but the
        //     frame stays OPEN across composite_glyphs calls, so a
        //     sequence like `cow_copy_area → composite_glyphs` would
        //     see the cow batch pending; flush it here defensively.
        self.flush_render_batch(store, platform, RenderFlushReason::Glyph)?;

        // (1) Resolve dst format gating — identical to legacy.
        let inner = match self.inner.as_mut() {
            Some(i) => i,
            None => return Err(RenderError::NoVk),
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let (dst_extent, dst_format, dst_depth) = {
            let d = store
                .get(dst_id)
                .ok_or(RenderError::UnknownDrawable(dst_id))?;
            (d.storage.extent, d.storage.format, d.storage.depth)
        };
        // BGRA8 window/pixmap mirrors, or a depth-8 R8 a8 mask
        // pixmap — the cairo/Pango component-alpha text
        // intermediate (glyph coverage accumulated with `op=Add`,
        // then composited onto the window; the i3-config-wizard
        // black-dialog path). Depth-1/4 R8 storages stay gated:
        // fractional glyph coverage in a bitmap has no defined
        // storage semantic here.
        let dst_supported = dst_format == vk::Format::B8G8R8A8_UNORM
            || (dst_format == vk::Format::R8_UNORM && dst_depth == 8);
        if !dst_supported {
            log::warn!(
                "render composite_glyphs (frame_builder): dst xid={:?} has format {:?} \
                 depth {dst_depth}; text pipeline supports B8G8R8A8_UNORM and \
                 depth-8 R8_UNORM — dropping run",
                store.get(dst_id).map(|d| d.xid),
                dst_format,
            );
            return Ok(stats);
        }
        // Same PictFormat-aware alpha classification the general
        // render_composite path uses — third pipeline-cache key
        // dimension (only DST_ALPHA-referencing ops care).
        let dst_has_alpha = dst_has_alpha_for_pict_format(dst_format, dst_depth, dst_pict_format);

        // (2) Lazy-init the atlas. The text pipelines this request
        //     needs are built at step (8a), once the per-glyph walk
        //     has said which layouts its entries carry — still at
        //     RECORD time, so emit can look them up immutably.
        if inner.glyph_atlas.is_none() {
            match GlyphAtlas::new(Arc::clone(&inner.vk)) {
                Ok(a) => inner.glyph_atlas = Some(a),
                Err(e) => {
                    log::error!(
                        "render composite_glyphs (frame_builder): GlyphAtlas::new failed: {e:?}"
                    );
                    return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
                }
            }
        }
        // A single request can mix glyph formats — the items stream's
        // inline `count == 255` element rewrites the active glyphset
        // mid-stream and glyphsets can differ in picture format — and
        // pipeline state is immutable, so the request is recorded as
        // one op per contiguous same-layout run. The pipelines those
        // runs bind are built after the per-glyph walk, from the
        // layouts the ATLAS ENTRIES actually carry; see step (8a).
        let component_alpha_supported = inner.vk.component_alpha_supported;

        // (2a) Atlas room, before the frame opens: when this request's
        //      misses (at their PACKED footprint) no longer fit, close
        //      the open frame and empty the atlas; every glyph in use
        //      re-uploads on its next draw.
        let fits = inner.glyph_atlas_misses_fit(glyphs.iter().map(|g| {
            let packed_w = match Self::effective_glyph_layout(
                g.pixels.source_format(),
                component_alpha_supported,
            ) {
                GlyphLayout::A8 => g.w,
                GlyphLayout::ComponentAlpha => {
                    g.w.saturating_mul(crate::kms::render::glyph_pixels::PLANES)
                }
            };
            (
                GlyphKey {
                    font_xid: g.gs_xid,
                    codepoint: g.glyph_id,
                },
                packed_w,
                g.h,
            )
        }));
        if !fits {
            self.reset_glyph_atlas(store, platform)?;
        }
        let inner = self.inner.as_mut().expect("inner");

        // (3) Open the frame if not open. `submit_group_ticket_or_open`
        //     either returns the existing shared ticket (if a sibling
        //     op already opened the group) or opens a fresh one.
        //
        //     Phase B.2 Mechanism 2: bump `acquire_generation` once at
        //     open and capture the resulting value on the OpenFrame.
        //     Every descriptor acquisition during the open frame uses
        //     this captured value; the close-path SubmittedOp uses the
        //     same value.
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

        // (4) Ticket-touch dst + snapshot prior ticket (first-touch
        //     only) + FIRST-TOUCH dst layout overlay (codex R1 finding
        //     3 fix — the overlay's pre_frame_layout is what
        //     `rollback_pre_submit` writes back on close-failure).
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

        // (5) Snapshot atlas prev ticket + atlas layout (first-touch
        //     only). The atlas snapshot is the rollback target if the
        //     close fails AFTER any upload op recorded; record_upload
        //     mutates `GlyphAtlas::current_layout` in place.
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

        // (6a) PRE-PASS — count UNIQUE atlas misses without packing or
        //      allocating. Codex R3 finding 2: a call with repeated
        //      uncached keys would otherwise count N misses where one
        //      upload suffices, triggering premature close+reopens.
        //      Dedupe keys against (a) the committed atlas, (b) the
        //      frame's already-queued pending_glyph_inserts, (c)
        //      prior misses in THIS pre-pass.
        let pending_pins_before_call = inner
            .frame_builder
            .open
            .as_ref()
            .map(|o| o.pins.len())
            .unwrap_or(0);
        let ceiling = inner.frame_builder.max_pinned_resources_per_frame();
        let mut prospective_miss_keys: HashSet<GlyphKey> = HashSet::new();
        for g in glyphs {
            let key = GlyphKey {
                font_xid: g.gs_xid,
                codepoint: g.glyph_id,
            };
            if g.w == 0 || g.h == 0 {
                continue;
            }
            // (a) committed atlas hit?
            if inner
                .glyph_atlas
                .as_ref()
                .expect("init")
                .lookup(key)
                .is_some()
            {
                continue;
            }
            // (b) pending insert already queued in the open frame?
            let pending_hit = inner.frame_builder.open.as_ref().is_some_and(|o| {
                o.pending_glyph_inserts
                    .entries
                    .iter()
                    .any(|(k, _)| *k == key)
            });
            if pending_hit {
                continue;
            }
            // (c) duplicate within this call?
            prospective_miss_keys.insert(key);
        }
        let prospective_misses = prospective_miss_keys.len();
        // Reserve one pin for the instance buffer that this call pins
        // unconditionally after the per-glyph walk (engine.rs:6768):
        // the pre-pass budgets prospective *uploads* only, so without
        // this `+ 1` a call whose uploads exactly fill `ceiling` ends
        // the frame at `ceiling + 1` pins (#137 step 1 / stage 2c).
        let needs_close_reopen = pending_pins_before_call + prospective_misses + 1 > ceiling;
        if needs_close_reopen {
            // Force a close+reopen NOW (pre-allocation). Log the
            // ceiling hit once per process via note_pin_ceiling_hit_once.
            // Report the same reserved total: this argument is the
            // attempted pin count, and the unreserved sum would
            // understate it.
            inner
                .frame_builder
                .note_pin_ceiling_hit_once(pending_pins_before_call + prospective_misses + 1);
            // Release the inner borrow before calling close_open_frame
            // (which itself reborrows self). Conventional cue without
            // invoking `drop()` on a reference.
            let _ = inner;
            self.close_open_frame(
                store,
                platform,
                crate::kms::render::frame_builder::CloseReason::PinCeiling,
            )?;
            // Re-open a fresh frame. Phase B.2 Mechanism 2: bump
            // acquire_generation at open and capture the value on
            // the fresh OpenFrame (same shape as the initial open
            // above).
            let new_ticket = platform.submit_group_ticket_or_open()?;
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation = inner.acquire_generation.saturating_add(1);
            let frame_generation = inner.acquire_generation;
            inner
                .frame_builder
                .open_for_paint(new_ticket, frame_generation);
            let frame_ticket_reopened = inner
                .frame_builder
                .open
                .as_ref()
                .expect("just opened")
                .ticket
                .clone();
            let dst_pre_layout_reopened = inner.current_layout_for_drawable(store, dst_id);
            let atlas_pre_layout_reopened = inner
                .glyph_atlas
                .as_ref()
                .map(crate::kms::render::glyph_atlas::GlyphAtlas::current_layout)
                .unwrap_or(vk::ImageLayout::UNDEFINED);
            let atlas_pre_ticket_reopened = inner
                .glyph_atlas
                .as_ref()
                .and_then(|a| a.last_render_ticket().cloned());
            let prior_dst_reopened = store.get(dst_id).and_then(|d| d.last_render_ticket.clone());
            {
                let open = inner.frame_builder.open.as_mut().expect("open");
                open.touched.first_touch(dst_id, prior_dst_reopened);
                open.layouts
                    .first_touch_drawable(dst_id, dst_pre_layout_reopened);
                open.atlas_prev_ticket_snapshot = Some(atlas_pre_ticket_reopened);
                open.layouts.first_touch_atlas(atlas_pre_layout_reopened);
            }
            store.touch_render_fence(dst_id, frame_ticket_reopened);
            // If the SINGLE call still exceeds the ceiling — drop
            // excess glyphs. The spec accepts atlas-slot leakage in
            // the rare-failure regime; we extend that to "pathological
            // single call". Same reservation as above: the unconditional
            // instance-buffer pin still needs its slot.
            if prospective_misses + 1 > ceiling {
                log::warn!(
                    "render composite_glyphs (frame_builder): single call requested {} \
                     atlas misses but per-frame ceiling is {}; will drop excess",
                    prospective_misses,
                    ceiling,
                );
            }
        }
        // Re-acquire `inner` for the per-glyph walk below. (Whether or
        // not we closed-and-reopened, the `inner` borrow was scoped.)
        let inner = self.inner.as_mut().expect("inner");
        // Recompute pending_pins_before_call AFTER any close+reopen.
        // On reopen, pins start at zero; without the recompute, the
        // per-glyph guard below would use the stale pre-close value
        // and prematurely drop glyphs (codex R3 finding 2a).
        let pending_pins_before_call = inner
            .frame_builder
            .open
            .as_ref()
            .map(|o| o.pins.len())
            .unwrap_or(0);

        // (6b) Per-glyph walk — actually upload the pixels + pack atlas
        //      slots for each miss. Deduplicate against (a) committed
        //      atlas, (b) pending_glyph_inserts in the open frame,
        //      (c) new_uploads already collected in this walk. Stop
        //      allocating once the ceiling is hit (drop excess glyphs).
        let mut glyphs_to_draw: Vec<crate::kms::render::frame_builder::RecordedTextGlyph> =
            Vec::with_capacity(glyphs.len());
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
        for g in glyphs {
            let key = GlyphKey {
                font_xid: g.gs_xid,
                codepoint: g.glyph_id,
            };
            // (a) committed atlas hit?
            let committed_hit = inner.glyph_atlas.as_ref().expect("init").lookup(key);
            // (b) pending-insert hit in the open frame?
            let pending_hit = inner.frame_builder.open.as_ref().and_then(|o| {
                o.pending_glyph_inserts
                    .entries
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, e)| *e)
            });
            // (c) new-uploads dedupe (same call earlier)?
            let dedupe_hit = new_uploads
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, e, _)| *e);
            let entry = if let Some(hit) = committed_hit.or(pending_hit).or(dedupe_hit) {
                hit
            } else {
                // Zero-size glyphs use a degenerate entry; no atlas
                // slot is consumed (the legacy path packs them anyway
                // but the returned slot is unused; we skip pack here
                // to avoid wasting one row on the packer).
                if g.w == 0 || g.h == 0 {
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
                // Pin-ceiling enforcement: check BEFORE calling
                // `pack()` so dropped glyphs don't leak atlas slots
                // (codex R4: pack consumes a shelf advance regardless
                // of whether the glyph ends up uploaded). The trailing
                // `+ 1` reserves the instance buffer's pin: when uploads
                // and the draw buffer cannot both fit, drop uploads to
                // leave room for the draw — losing a glyph loses one
                // glyph, losing the draw loses the whole run.
                if new_uploads.len() + 1 + pending_pins_before_call + 1 > ceiling {
                    stats.glyphs_dropped += 1;
                    continue;
                }
                // Resolve to dense A8 BEFORE pack() to avoid leaking a
                // packed slot on malformed input (codex R5). A1 wire is
                // expanded here — on the atlas-MISS path only, so a glyph
                // already resident in the atlas never re-expands (#2,
                // 2026-07-08 render-optimization gaps).
                //
                // The layout is decided FIRST, because it selects
                // both the bytes staged and the atlas footprint
                // reserved. `ComponentAlpha` stages four adjacent
                // coverage planes (logical R, G, B, A) and reserves
                // `4 * w` texels; `A8` stages one plane at `w`,
                // reducing an ARGB32 source to the mean of its
                // colour channels — the `dualSrcBlend`-less
                // fallback.
                let layout = Self::effective_glyph_layout(
                    g.pixels.source_format(),
                    component_alpha_supported,
                );
                let Some(atlas_bytes) = g.pixels.to_atlas_bytes(g.w, g.h, layout) else {
                    log::warn!(
                        "render composite_glyphs (frame_builder): glyph pixels too short \
                         for {}x{} at layout {layout:?}; dropping pre-pack",
                        g.w,
                        g.h,
                    );
                    stats.glyphs_dropped += 1;
                    continue;
                };
                let copy_len = atlas_bytes.len();
                // The PACKED footprint — what the shelf packer
                // reserves and what the upload copy region covers.
                // The dst quad, the damage extent and the instance
                // geometry all take `g.w` instead; feeding the packed
                // width to any of those stretches the glyph across
                // its own planes (design invariant 9).
                let packed_w = match layout {
                    GlyphLayout::A8 => g.w,
                    GlyphLayout::ComponentAlpha => {
                        g.w.saturating_mul(crate::kms::render::glyph_pixels::PLANES)
                    }
                };
                let Some((atlas_x, atlas_y)) = inner
                    .glyph_atlas
                    .as_mut()
                    .expect("init")
                    .pack(packed_w, g.h)
                else {
                    inner
                        .glyph_atlas
                        .as_mut()
                        .expect("init")
                        .note_dropped(packed_w, g.h);
                    stats.glyphs_dropped += 1;
                    continue;
                };
                stats.atlas_interns += 1;
                // Pinned into the open frame here; the GlyphUpload op that
                // replays it is recorded in (6c) below, in the same frame.
                let upload_pin = inner.upload_to_frame(
                    &atlas_bytes[..copy_len],
                    inner.upload_copy_align,
                    crate::kms::vk::mem_accounting::ChurnClass::GlyphUpload,
                )?;
                debug_assert_eq!(
                    copy_len,
                    (packed_w as usize) * (g.h as usize),
                    "the staged bytes must exactly fill the reserved atlas footprint",
                );
                let new_entry = AtlasEntry {
                    atlas_x,
                    atlas_y,
                    packed_w,
                    logical_w: g.w,
                    h: g.h,
                    pen_left: 0,
                    pen_top: 0,
                    layout,
                };
                new_uploads.push((key, new_entry, upload_pin));
                stats.glyph_uploads += 1;
                new_entry
            };
            if entry.logical_w == 0 || entry.h == 0 {
                continue;
            }
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
                // From the ATLAS ENTRY, never recomputed from the
                // input: the entry is what the pipeline will sample,
                // and for a glyph that interned in an EARLIER request
                // it is the only authority on how it was packed.
                layout: entry.layout,
            });
        }

        if glyphs_to_draw.is_empty() && new_uploads.is_empty() && new_zero_inserts.is_empty() {
            return Ok(stats);
        }

        // (6c) Commit new uploads + zero-inserts + glyph_uploads
        //      counter. Pin-ceiling enforcement happened in pre-pass +
        //      per-glyph drop above; we know
        //      new_uploads.len() ≤ ceiling - pending.
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

        if glyphs_to_draw.is_empty() {
            return Ok(stats);
        }

        // (8a) Text pipelines for exactly the layouts this request's
        //      glyphs turned out to have. Built at RECORD time, so
        //      emit only ever looks an entry up.
        //
        //      Derived from the recorded glyphs — i.e. from the ATLAS
        //      ENTRIES — and not from the inputs' source formats. The
        //      two agree today, because `component_alpha_supported`
        //      is fixed for the life of the device, so an entry's
        //      layout is always what `effective_glyph_layout` answers
        //      for its glyphset's format. But it is the entry the run
        //      binds a pipeline for, and an entry can have interned
        //      in an EARLIER request; keying this off the protocol
        //      tags would let emit look up a pipeline nobody built.
        //
        //      This sits after the per-glyph walk and before the
        //      runs, which leaves the close+reopen decision exactly
        //      where it was (invariant 7c) — a pipeline build is not
        //      a frame op.
        for layout in [GlyphLayout::A8, GlyphLayout::ComponentAlpha] {
            if !glyphs_to_draw.iter().any(|g| g.layout == layout) {
                continue;
            }
            inner.ensure_text_pipeline(
                op,
                dst_format,
                dst_has_alpha,
                layout == GlyphLayout::ComponentAlpha,
                "composite_glyphs (frame_builder)",
            )?;
        }

        // (7) Build the clip scissor list. #133 step 3 (P4): the inline
        // copy of `build_render_clip_scissors` was replaced by the shared
        // bounds-aware helper — same arithmetic, with the destination
        // handle's content bounds in place of the raw storage extent, so
        // a RENDER glyph draw cannot reach a bordered window's ring even
        // with no picture clip of its own.
        let clip_scissors: Vec<vk::Rect2D> =
            build_render_clip_scissors_to(clip_rects, dst.bounds_in(dst_extent));
        if clip_scissors.is_empty() {
            return Ok(stats);
        }

        // (8) Append-time damage mutation. Spec § "Damage accumulation"
        //     mandates append-time mutation (the X11 client's request
        //     already happened the moment the server accepted it;
        //     DamageNotify fires on acceptance, before GPU work).
        //     Frame failure does NOT roll damage back — restoration
        //     would lose a DamageNotify the client has already been
        //     told about.
        if damage_max_x > damage_min_x && damage_max_y > damage_min_y {
            let dx = damage_min_x.max(0);
            let dy = damage_min_y.max(0);
            let w = u32::try_from(damage_max_x - dx).unwrap_or(0);
            let h = u32::try_from(damage_max_y - dy).unwrap_or(0);
            if w > 0 && h > 0 {
                store.damage(
                    dst_id,
                    clamp_rect_to(
                        vk::Rect2D {
                            offset: vk::Offset2D { x: dx, y: dy },
                            extent: vk::Extent2D {
                                width: w,
                                height: h,
                            },
                        },
                        dst.bounds_in(dst_extent),
                    ),
                );
            }
        }

        // (8b/9) Form the draw runs and hand them to the shared run
        //      recorder, which builds + pins ONE instance buffer for
        //      the whole request and appends one op per run. Runs are
        //      contiguous stretches of equal effective `GlyphLayout`
        //      in REQUEST ORDER — a request can switch glyphset
        //      mid-stream and glyphsets can differ in format, and
        //      pipeline state is immutable, so a mixed request needs
        //      one op per stretch. Never regrouped by format: see
        //      `split_glyph_runs`.
        //
        //      On a device WITHOUT `dualSrcBlend` there is exactly
        //      one run whatever the request mixes: the upload reduces
        //      every ARGB32 glyph to a single A8 plane, so every
        //      glyph has the same effective layout and the split is
        //      inert. With it, an alternating stream really does
        //      record several ops.
        //
        //      Whatever the run count, the clip region built above,
        //      the damage union mutated above and the
        //      `mark_contents_modified` below stay ONE per request: a
        //      client sent one request and must see one damage event
        //      and one region for it.
        let runs = Self::split_glyph_runs(&glyphs_to_draw);
        let recorded_instances = Self::record_glyph_runs(
            inner,
            &runs,
            &GlyphRunCommon {
                dst_id,
                dst_old_layout: dst_pre_frame_layout,
                op,
                dst_has_alpha,
                foreground_rgba,
                clip_scissors,
            },
        )?;
        if recorded_instances == 0 {
            return Ok(stats);
        }
        // Request-wide, one per request however many runs were
        // recorded (spec stage 2b).
        store.mark_contents_modified(dst_id);

        // (10) Do NOT auto-close. Frame closes via M2 (next non-ported
        //      op), M3 (maybe_composite), timeout, sync_wait, or
        //      shutdown.
        Ok(stats)
    }

    /// The atlas layout a glyph of `source` effectively gets — the
    /// value written into its [`AtlasEntry`] when it interns, and the
    /// value [`Self::split_glyph_runs`] groups on.
    ///
    /// **Derived here, not tagged by the parser.** The backend's
    /// items parse tags each [`CompositeGlyphInput`] with the
    /// glyphset's picture format, which is protocol state. Turning
    /// that into a layout depends on `component_alpha_supported`
    /// (device state the protocol layer has no business reading) and
    /// on what the atlas upload actually stores — so it lives here,
    /// beside atlas allocation and pipeline choice, where the three
    /// cannot drift apart.
    ///
    /// **`component_alpha_supported` is the device half of the
    /// answer, and this is the ONLY place it is consulted for
    /// glyphs.** `dualSrcBlend` is an optional Vulkan 1.0 feature
    /// (Broadcom V3D ships without it) and the `SRC1_*` blend factors
    /// the component-alpha path needs are unavailable without it. So
    /// where it is absent, an ARGB32 glyph is REDUCED to one
    /// grayscale coverage plane at upload
    /// ([`reduce_argb32_glyph_to_a8_coverage`](crate::kms::render::glyph_pixels::reduce_argb32_glyph_to_a8_coverage))
    /// and is genuinely an A8 entry in the atlas — which is exactly
    /// the grayscale AA `vk/device.rs` already promises there.
    ///
    /// Deciding it here — once, beside atlas allocation and pipeline
    /// choice — is what keeps the three from drifting: the layout
    /// selects the packed width the uploader stages, the pipeline the
    /// emit binds and the run boundaries the splitter cuts, and an
    /// entry tagged `ComponentAlpha` over a single-plane upload would
    /// be sampled as garbage. It is also why the fallback needs no
    /// runtime switch: `component_alpha_supported` is fixed for the
    /// life of the device, so the atlas never holds mixed state, and
    /// the reduction is a pure function testable on its own
    /// (`feedback_no_feature_kill_switches`).
    ///
    /// One clean consequence worth naming: on a device without
    /// `dualSrcBlend` every glyph has the same effective layout, so a
    /// mixed-format request forms exactly ONE run and
    /// [`Self::split_glyph_runs`] is inert.
    pub(super) fn effective_glyph_layout(
        source: GlyphSourceFormat,
        component_alpha_supported: bool,
    ) -> GlyphLayout {
        match source {
            // A8 is already a coverage plane; A1 expands into one.
            GlyphSourceFormat::A8 | GlyphSourceFormat::A1 => GlyphLayout::A8,
            // ARGB32 carries per-channel coverage on the wire (or
            // colour, for an emoji glyphset — Xorg's `NeedsComponent`
            // makes no attempt to tell them apart and neither do we).
            // Four packed planes where the device can blend them,
            // the grayscale mean where it cannot.
            GlyphSourceFormat::Argb32 => {
                if component_alpha_supported {
                    GlyphLayout::ComponentAlpha
                } else {
                    GlyphLayout::A8
                }
            }
        }
    }

    /// Split `glyphs` into contiguous runs — one per maximal stretch
    /// of equal effective [`GlyphLayout`] — **in request order**.
    ///
    /// The layout is read off each glyph
    /// ([`RecordedTextGlyph::layout`]) rather than from a parallel
    /// slice the caller has to keep in lockstep: a second sequence
    /// indexed by glyph position is free to drift from it, and the
    /// drift is silent (a glyph sampled by the wrong pipeline).
    ///
    /// Glyphs of different layouts need different pipelines, pipeline
    /// state is immutable, and one recorded op is one `vkCmdDraw`
    /// with one pipeline — so a request that mixes formats has to
    /// become several ops (spec stage 2b).
    ///
    /// **Contiguous, never regrouped.** Gathering all the A8 glyphs
    /// into one run and all the component-alpha glyphs into another
    /// is the obvious optimisation and it is wrong: PictOps are not
    /// commutative in general, so reordering changes pixels wherever
    /// two glyph quads overlap — and overlap is ordinary (kerning,
    /// italics, combining marks), not exotic. Only `Add` would be
    /// safe to reorder, and special-casing one op would leave a
    /// reordering splitter in the tree for every other op to trip
    /// over.
    ///
    /// The cost of that is bounded and known: a stream alternating
    /// format every glyph degrades to one draw per glyph, which is
    /// what Xorg does on this path anyway — `render/glyph.c` issues
    /// one `CompositePicture` per glyph for `maskFormat == 0`, so our
    /// worst case is its normal case. Real clients switch glyphset
    /// per font or AA change, so the common case stays one run.
    pub(super) fn split_glyph_runs(
        glyphs: &[crate::kms::render::frame_builder::RecordedTextGlyph],
    ) -> Vec<&[crate::kms::render::frame_builder::RecordedTextGlyph]> {
        let mut runs: Vec<&[crate::kms::render::frame_builder::RecordedTextGlyph]> = Vec::new();
        let mut start = 0usize;
        for i in 1..glyphs.len() {
            if glyphs[i].layout != glyphs[i - 1].layout {
                runs.push(&glyphs[start..i]);
                start = i;
            }
        }
        if start < glyphs.len() {
            runs.push(&glyphs[start..]);
        }
        runs
    }

    /// Build ONE instance vertex buffer covering `runs` (contiguous
    /// glyph slices, in request order) and append one
    /// `RecordedCompositeGlyphs` op per run, each carrying its own
    /// `(first_instance, instance_count)` range into that buffer.
    /// Returns the total number of instances recorded — zero means
    /// nothing was appended and no pin was taken.
    ///
    /// **One shared buffer, not one per run.** The frame-pin ceiling
    /// budgets glyph *uploads* in a pre-pass and then reserves exactly
    /// **one** further pin for the draw buffer (#137 step 1 / spec
    /// stage 2c). A buffer per run would make that reservation depend
    /// on a run count the pre-pass does not know yet, so the pre-pass
    /// would have to move or run twice. Pinning once, whatever the run
    /// count, is what keeps that arithmetic true — so this function is
    /// the only place a `CompositeGlyphs` instance buffer is pinned.
    ///
    /// **Runs arrive already cut, from [`Self::split_glyph_runs`].**
    /// Glyphs of different `GlyphLayout`s need different pipelines
    /// and pipeline state is immutable, so a request that mixes
    /// formats is recorded as several contiguous runs (spec stage
    /// 2b). This function only ranges them over one buffer: the clip
    /// region, the damage union and the `mark_contents_modified` stay
    /// with the caller, one per request however many runs it hands
    /// over. Production hands over exactly one while the upload
    /// reduces ARGB32 to A8, so a test that hands over two is still
    /// driving the code that ships.
    ///
    /// Instance data carries dst rects + atlas TEXEL coords; the
    /// shader normalizes UV by the atlas extent (a per-run push
    /// constant at emit), so recorded data survives a future atlas
    /// grow/repack. The buffer is pinned into the open frame exactly
    /// like the trapezoid path so it outlives the deferred submit.
    ///
    /// # Errors
    ///
    /// `Vk(...)` if the staging buffer cannot be allocated.
    pub(super) fn record_glyph_runs(
        inner: &mut RenderEngineInner,
        runs: &[&[crate::kms::render::frame_builder::RecordedTextGlyph]],
        common: &GlyphRunCommon,
    ) -> Result<u32, RenderError> {
        type Instance = crate::kms::vk::text_pipeline::GlyphInstanceData;
        let stride = std::mem::size_of::<Instance>();
        let total_glyphs: usize = runs.iter().map(|r| r.len()).sum();
        let mut instance_data: Vec<u8> = Vec::with_capacity(total_glyphs * stride);
        // (first_instance, instance_count, layout) per run. A glyph
        // whose geometry does not fit an instance is dropped, so a
        // run's count is what actually landed in the buffer, not its
        // glyph count — and the next run's `first_instance` follows
        // the bytes, never the glyph index.
        //
        // The layout comes off the run's first glyph: the splitter
        // cut the run precisely so every glyph in it agrees, and it
        // selects BOTH the pipeline the recorded op will bind and
        // each instance's packed plane stride.
        let mut ranges: Vec<(u32, u32, GlyphLayout)> = Vec::with_capacity(runs.len());
        for run in runs {
            let first_instance = u32::try_from(instance_data.len() / stride).unwrap_or(0);
            let Some(layout) = run.first().map(|g| g.layout) else {
                continue;
            };
            for g in *run {
                debug_assert_eq!(
                    g.layout, layout,
                    "a recorded glyph run must be homogeneous in effective layout",
                );
                if let Some(inst) = Instance::from_glyph(
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
            let end = u32::try_from(instance_data.len() / stride).unwrap_or(0);
            let instance_count = end.saturating_sub(first_instance);
            if instance_count > 0 {
                ranges.push((first_instance, instance_count, layout));
            }
        }
        let total_instances = u32::try_from(instance_data.len() / stride).unwrap_or(0);
        if total_instances == 0 || ranges.is_empty() {
            return Ok(0);
        }

        // Exactly one pin for the whole request — the pin the ceiling
        // reserved.
        let instance_pin = inner.upload_to_frame(
            &instance_data,
            UPLOAD_VERTEX_ALIGN,
            crate::kms::vk::mem_accounting::ChurnClass::GlyphRun,
        )?;
        let open = inner.frame_builder.open.as_mut().expect("open");

        // No damage_rect carried on any run: damage is mutated at
        // append time by the caller, once for the whole request.
        for (run_idx, (first_instance, instance_count, layout)) in ranges.into_iter().enumerate() {
            // Run 0 finds the destination in the frame's pre-op
            // layout; every later run finds it as the run before left
            // it, and `record_text_run_scissored` ends in
            // SHADER_READ_ONLY_OPTIMAL. Carrying the pre-op layout on
            // run 2+ would declare a wrong `oldLayout` in its
            // barrier — and a pre-op `UNDEFINED` would license the
            // driver to discard the earlier runs' pixels.
            let dst_old_layout = if run_idx == 0 {
                common.dst_old_layout
            } else {
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
            };
            open.push_op_and_set_layouts(
                crate::kms::render::frame_builder::RecordedOp::CompositeGlyphs(
                    crate::kms::render::frame_builder::RecordedCompositeGlyphs {
                        dst_id: common.dst_id,
                        dst_old_layout,
                        op: common.op,
                        dst_has_alpha: common.dst_has_alpha,
                        foreground_rgba: common.foreground_rgba,
                        component_alpha: layout == GlyphLayout::ComponentAlpha,
                        instance_pin,
                        first_instance,
                        instance_count,
                        clip_scissors: common.clip_scissors.clone(),
                        damage_rect: None,
                    },
                ),
                &[(common.dst_id, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }
        Ok(total_instances)
    }
}

/// #137 tier 1 — admit a `ResolvedSource::Drawable` glyph source whose
/// *sampled domain* is a single pixel under a repeat that maps that
/// pixel over the whole plane. Such a picture is a constant colour by
/// definition, so collapsing it to `foreground_premul` is **exact**,
/// not an approximation: the same answer Xorg's per-glyph
/// `Composite(op, pSrc, glyphPicture, pDst)` computes
/// (`../xserver/render/glyph.c:575` `miGlyphs`), by a cheaper route.
///
/// Java2D paints all text through `XRSolidSrcPict` — a 1x1 pixmap
/// picture with `repeat=Normal` — rather than `CreateSolidFill`, which
/// is why every Java/AWT text draw was discarded.
///
/// Returns the 1x1 read rect **in backing (storage) space**, or `None`
/// when the source is not of that shape and must keep dropping.
///
/// The four admission rules, each of which has a way to be got wrong
/// that no simple test would catch:
///
/// - **`mask_fmt == 0` only.** That is the branch Java takes, and the
///   one whose reference behaviour is per-glyph compositing. For
///   `mask_format != 0` Xorg accumulates an A8 mask and composites
///   once; `render_composite_glyphs` takes the parameter as
///   `_mask_fmt` and ignores it, always taking the per-glyph shortcut.
///   That is a known deviation, and admitting a new class of source
///   into it would broaden incorrect behaviour rather than fix
///   anything.
/// - **The sampled DOMAIN, not the backing storage.** A window picture
///   can resolve to a 1x1 logical domain sitting inside a much larger
///   redirected backing ([`SourceDrawable::content`]); testing the
///   storage extent would reject exactly that case, which is the one a
///   naive pixmap-only test still passes.
/// - **`Repeat::None` is not admissible even at 1x1.** Outside the
///   single pixel the source reads as transparent, so the correct
///   result paints only the glyph area overlapping that one pixel.
///   Collapsing it to a colour would paint every glyph.
///   `Normal`/`Pad`/`Reflect` all map one pixel onto the whole plane,
///   so all three are exact.
/// - **The rect must lie inside the storage.** [`Src::server_internal`]
///   is an unclipped backing-space handle and `get_image` clamps, so an
///   offset outside the allocation would silently read nothing; drop
///   instead.
///
/// A source `PictTransform` needs no gate: with a one-pixel domain and
/// a plane-covering repeat every source coordinate maps onto that same
/// pixel, whatever the matrix.
pub(crate) fn uniform_pixel_glyph_source(
    src: SourceDrawable,
    repeat: Repeat,
    storage: vk::Extent2D,
    mask_fmt: u32,
) -> Option<vk::Rect2D> {
    if mask_fmt != 0 {
        return None;
    }
    match repeat {
        Repeat::Normal | Repeat::Pad | Repeat::Reflect => {}
        Repeat::None => return None,
    }
    let domain = src.domain().unwrap_or(storage);
    if domain.width != 1 || domain.height != 1 {
        return None;
    }
    let (x, y) = src.offset();
    if x < 0
        || y < 0
        || u32::try_from(x).ok()? >= storage.width
        || u32::try_from(y).ok()? >= storage.height
    {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D { x, y },
        extent: vk::Extent2D {
            width: 1,
            height: 1,
        },
    })
}
