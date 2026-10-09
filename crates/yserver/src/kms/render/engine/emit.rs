use super::*;

/// Phase B.1 Task 12: replay a single `RecordedOp` into `cb`. Caller
/// holds `&mut inner` and `&mut store`; this function consumes the
/// recorder-side state captured at append-time and emits the GPU
/// commands necessary to honour it.
pub(super) fn emit_recorded_op_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    pins: &crate::kms::render::frame_builder::FramePinSet,
    frame_generation: u64,
    op: &crate::kms::render::frame_builder::RecordedOp,
) -> Result<(), RenderError> {
    use crate::kms::render::frame_builder::RecordedOp as Op;
    match op {
        Op::GlyphUpload(up) => {
            let atlas = inner.glyph_atlas.as_mut().ok_or(RenderError::NoVk)?;
            let src = pins.upload_slices[up.upload_pin.0 as usize];
            atlas.record_upload(
                cb,
                src.buffer,
                src.offset,
                up.atlas_x,
                up.atlas_y,
                up.packed_w,
                up.h,
            );
            Ok(())
        }
        Op::CompositeGlyphs(cg) => {
            // SLICE2: glyph pass-split deferred to Phase 4 (text.rs owns its pass)
            let atlas_extent = inner
                .glyph_atlas
                .as_ref()
                .ok_or(RenderError::NoVk)?
                .extent();
            // Clone the Vk handle owner so the recorder call doesn't
            // alias the pipeline cache against `&inner.vk`.
            let vk = inner.vk.clone();
            // Per-glyph instance vertex buffer pinned at record time (#1).
            let instance = pins.upload_slices[cg.instance_pin.0 as usize];
            let drawable = store
                .get_mut(cg.dst_id)
                .ok_or(RenderError::UnknownDrawable(cg.dst_id))?;
            let mut adapter = StorageTextTarget {
                extent: drawable.storage.extent,
                image: drawable.storage.image,
                image_view: drawable.storage.image_view,
                current_layout: cg.dst_old_layout,
            };
            // Per-(op, dst_format, dst_has_alpha) pipeline — the
            // entry was built at record time by
            // `ensure_text_pipeline`, so a miss here is a logic bug
            // (surface as NoVk rather than panicking mid-emit).
            let pipeline = inner
                .text_pipelines
                .get(&(
                    cg.op,
                    drawable.storage.format,
                    cg.dst_has_alpha,
                    cg.component_alpha,
                ))
                .ok_or(RenderError::NoVk)?;
            crate::kms::vk::ops::text::record_text_run_scissored(
                &vk,
                cb,
                &mut adapter,
                atlas_extent,
                pipeline,
                instance.buffer,
                instance.offset,
                cg.first_instance,
                cg.instance_count,
                cg.foreground_rgba,
                &cg.clip_scissors,
            )?;
            // Pipeline borrow ends here; mutate storage now.
            drawable.storage.current_layout = adapter.current_layout;
            Ok(())
        }
        Op::LayoutTransition(lt) => {
            let drawable = store
                .get_mut(lt.drawable_id)
                .ok_or(RenderError::UnknownDrawable(lt.drawable_id))?;
            drawable.record_layout_transition(
                &inner.vk,
                cb,
                lt.target_layout,
                lt.src_stage,
                lt.src_access,
                lt.dst_stage,
                lt.dst_access,
            );
            Ok(())
        }
        Op::RenderComposite(rc) => emit_recorded_render_composite_into_cb(inner, cb, pins, rc),
        // Phase B.3 — CopyArea implemented in Task 2; stubs for later tasks.
        Op::CopyArea(ca) => emit_recorded_copy_area_into_cb(inner, cb, ca),
        Op::PutImage(pi) => emit_recorded_put_image_into_cb(inner, cb, pins, pi),
        Op::FillRect(fr) => emit_recorded_fill_rect_into_cb(inner, store, cb, fr),
        Op::LogicFill(lf) => emit_recorded_logic_fill_into_cb(inner, store, cb, lf),
        Op::ImageText(it) => emit_recorded_image_text_into_cb(inner, store, cb, pins, it),
        Op::RenderTrapsOrTris(rt) => {
            emit_recorded_render_traps_or_tris_into_cb(inner, store, cb, pins, frame_generation, rt)
        }
        Op::MaskedCopyArea(m) => {
            emit_recorded_masked_copyarea_into_cb(inner, cb, frame_generation, m)
        }
        Op::ClipSnapshotRefresh(r) => emit_recorded_clip_snapshot_refresh_into_cb(inner, cb, r),
    }
}

/// Phase B.2 Task 12: replay a deferred `RecordedRenderComposite`
/// into the frame's command buffer. Mirrors `render_composite_legacy`'s
/// CB-recording shape (lines ~6200-6280) BUT:
///
/// - takes the dst's old layout from the **recorded payload** rather
///   than `Drawable::storage.current_layout` (Pitfall 5 — the latter is
///   stale during deferred recording across multiple ops in one frame),
/// - operates against a [`RecordedCompositeTarget`] adapter that holds
///   the pre-resolved image / view / extent (no `&mut DrawableStore`
///   read; the descriptor + views were resolved at append-time and
///   pinned by the frame).
///
/// The barrier emission is **identical** to the legacy path: exactly
/// one `to_color` (open) + one `to_read` (close). No double-barrier,
/// no manual barrier outside the recorder helpers. See plan §Task 12
/// Step 4 + Pitfall 5+6.
fn emit_recorded_render_composite_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    _pins: &crate::kms::render::frame_builder::FramePinSet,
    rc: &crate::kms::render::frame_builder::RecordedRenderComposite,
) -> Result<(), RenderError> {
    use crate::kms::vk::{
        ops::render as vk_render,
        render_pipeline::{StdPictOp, record_solid_color_clear},
    };

    // (1) Synthetic 1×1 src/mask clears (`record_solid_color_clear`
    //     internally transitions the scratch to SHADER_READ_ONLY).
    //     Per Pitfall 4b, the engine-owned `solid_src_image` /
    //     `solid_mask_image` are never grown — the same `SolidColorImage`
    //     handles the descriptor write at op-append captured. The clear
    //     fires per-op at emit time, rewriting the 1×1 texel for THIS
    //     op's source colour.
    if let Some(c) = rc.src_clear_color {
        let solid = inner.solid_src_image.as_mut().expect(
            "solid_src_image: ensure_render_assets ran in render_composite_via_frame_builder",
        );
        record_solid_color_clear(&inner.vk, cb, solid, c);
    }
    if let Some(c) = rc.mask_clear_color {
        let solid = inner.solid_mask_image.as_mut().expect(
            "solid_mask_image: ensure_render_assets ran in render_composite_via_frame_builder",
        );
        record_solid_color_clear(&inner.vk, cb, solid, c);
    }

    // (2) Self-alias copy: dst → src_alias_readback scratch. Same as
    //     legacy `render_composite_legacy` lines ~6223-6230. The copy
    //     RESTORES dst's old layout after the transfer (per
    //     `DstReadback::record_copy_from`'s contract), so the subsequent
    //     `to_color` open barrier sees the same `dst_old_layout` it
    //     would have seen without the scratch path.
    //
    //     Pitfall 4: under B.2 grow semantics, the `src_alias_readback`
    //     here is the SAME `DstReadback` instance the op-append site
    //     resolved its view against. Growth-during-frame is handled by
    //     the via_fb path's "close + grow + adopt + reopen" sequence
    //     before this emit runs.
    if rc.src_alias_view.is_some() {
        let rb = inner.src_alias_readback.as_mut().expect(
            "src_alias_readback: ensured at op-append in render_composite_via_frame_builder",
        );
        rb.record_copy_from(
            cb,
            rc.dst_image,
            rc.dst_old_layout,
            rc.dst_format,
            rc.dst_extent,
        );
    }

    // (2b) Shader-side dst readback copy: Saturate and the
    //      Disjoint/Conjoint families bind binding 2 (`dst_tex`) and
    //      expect it to contain a snapshot of dst before this op. The
    //      append path only ensures/resolves the scratch view and writes
    //      the descriptor; the actual transition+copy must replay here,
    //      in command-buffer order, before the draw samples it.
    if rc.needs_dst_readback {
        let rb = inner
            .dst_readback
            .as_mut()
            .expect("dst_readback: ensured at op-append in render_composite_via_frame_builder");
        rb.record_copy_from(
            cb,
            rc.dst_image,
            rc.dst_old_layout,
            rc.dst_format,
            rc.dst_extent,
        );
    }

    // (3) Pipeline lookup. The cache `get` takes `&mut self`; the borrow
    //     is released before the open barrier emission so `&inner.vk`
    //     can be re-borrowed safely.
    let std_op = StdPictOp::from_u8(rc.op).expect("op validated at append in via_frame_builder");
    let pipeline = inner
        .render_pipelines
        .as_mut()
        .expect("render_pipelines: ensured at op-append")
        .get(
            std_op,
            rc.dst_format,
            rc.dst_has_alpha,
            rc.mask_component_alpha,
        )
        .map_err(|e| {
            log::warn!("emit_recorded_render_composite: pipeline get failed: {e:?}");
            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
        })?;
    let pipeline_layout = inner
        .render_pipelines
        .as_ref()
        .expect("render_pipelines: ensured at op-append")
        .pipeline_layout();

    // (4) Open: emit the dst `to_color` barrier using the pre-resolved
    //     overlay-driven old layout. Pitfall 5 — `record_render_composite_open`
    //     reads `dst.current_layout()` which is `storage.current_layout`
    //     (stale across multi-op frames); the `_with_old_layout` overload
    //     takes `old_layout` explicitly and does NOT mutate the target.
    let target = RecordedCompositeTarget {
        image: rc.dst_image,
        view: rc.dst_view,
        extent: rc.dst_extent,
    };
    vk_render::record_render_composite_open_with_old_layout(
        &inner.vk,
        cb,
        &target,
        pipeline,
        rc.dst_old_layout,
    )
    .map_err(RenderError::Vk)?;

    // (5) Per-rect draws. clip_rects=None → single full-extent scissor
    //     (matches legacy `build_render_clip_scissors`'s None branch).
    //     The full-extent fallback's `vk::Rect2D` is locally owned so
    //     its borrow lifetime is the function scope.
    let full_extent_scissor;
    // #133 step 3 (P4): the recorded content bounds stand in for the
    // storage extent, so a deferred composite is scissored to the
    // drawable's content even with no picture clip of its own.
    let dst_bounds = resolve_recorded_bounds(rc.dst_bounds, rc.dst_extent);
    let clip_scissors: &[vk::Rect2D] = match rc.clip_rects.as_deref() {
        Some(cr) => {
            // Important distinction:
            // - `None` => no picture clip, paint everywhere
            // - `Some([])` => empty picture clip, paint nothing
            //
            // B.2 originally collapsed both into the same fallback,
            // which let replayed ops redraw whole-frame damage after
            // `SetPictureClipRectangles(n=0)`.
            let owned = build_render_clip_scissors_to(Some(cr), dst_bounds);
            if owned.is_empty() {
                let mut target = target;
                vk_render::record_render_composite_close(&inner.vk, cb, &mut target);
                return Ok(());
            }
            full_extent_scissor = owned;
            full_extent_scissor.as_slice()
        }
        None => {
            full_extent_scissor = vec![dst_bounds];
            full_extent_scissor.as_slice()
        }
    };
    vk_render::record_render_composite_draws(
        &inner.vk,
        cb,
        pipeline_layout,
        rc.descriptor_set,
        rc.dst_extent,
        &rc.attrs,
        &rc.rects,
        clip_scissors,
    );

    // (6) Close: emit `cmd_end_rendering` + dst `to_read` barrier back to
    //     `SHADER_READ_ONLY_OPTIMAL`. The recorder calls
    //     `target.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` —
    //     intentional no-op on `RecordedCompositeTarget` (Pitfall 4b
    //     audit); storage layout commit happens via
    //     `commit_close_success`'s overlay walk on submit success.
    let mut target = target;
    vk_render::record_render_composite_close(&inner.vk, cb, &mut target);

    Ok(())
}

/// Phase B.3 Task 2 (N1, N8): replay a deferred `RecordedCopyArea` into the
/// frame's command buffer. Mirrors the legacy `copy_area` barrier shapes
/// EXACTLY: self-overlap path mirrors engine.rs:2814-2918 (three-barrier
/// sequence); disjoint path mirrors engine.rs:2951-3045 (two-barrier
/// sequence). Terminal layout for BOTH src and dst is
/// `SHADER_READ_ONLY_OPTIMAL` (N1 single-terminal-layout rule).
///
/// The exact stage/access masks mirror the legacy paths: the producer mask
/// (src_access on pre-barriers) is `SHADER_SAMPLED_READ | TRANSFER_WRITE |
/// COLOR_ATTACHMENT_WRITE` to drain prior compose/fill/put-image writes on the
/// same image — a simpler `TRANSFER_WRITE only` mask would recreate the
/// B.2-class RAW hazard.
///
/// The `self_overlap_scratch` image in the payload is allocated by the
/// `copy_area` append path (N8 allocation-first) and owned by
/// `RecordedCopyArea::self_overlap_scratch` until the close-path scratch walk
/// moves it into `SubmittedOp::scratch`. This function READS the scratch
/// but does NOT mutate its ownership — `ca` is `&RecordedCopyArea` (not `&mut`).
fn emit_recorded_copy_area_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    ca: &crate::kms::render::frame_builder::RecordedCopyArea,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    if let Some(scratch) = ca.self_overlap_scratch.as_ref() {
        // Self-overlap: mirror engine.rs:2814-2918's three-barrier sequence.
        // (1) src → TRANSFER_SRC_OPTIMAL (drains prior compose/fill/put-image writes).
        barrier_to_layout(
            device,
            cb,
            ca.src_image,
            ca.src_old_layout,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );
        // (2) scratch UNDEFINED → TRANSFER_DST_OPTIMAL.
        barrier_to_layout(
            device,
            cb,
            scratch.image,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::PipelineStageFlags2::TOP_OF_PIPE,
            vk::AccessFlags2::empty(),
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
        );
        // Copy src_rect → scratch (at offset 0,0).
        let region1 = [vk::ImageCopy::default()
            .src_subresource(color_layers())
            .src_offset(vk::Offset3D {
                x: ca.src_rect.offset.x,
                y: ca.src_rect.offset.y,
                z: 0,
            })
            .dst_subresource(color_layers())
            .dst_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(vk::Extent3D {
                width: ca.dst_rect.extent.width,
                height: ca.dst_rect.extent.height,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image(
                cb,
                ca.src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                scratch.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region1,
            );
        }
        // (3a) scratch TRANSFER_DST → TRANSFER_SRC.
        barrier_to_layout(
            device,
            cb,
            scratch.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );
        // (3b) src (== dst) TRANSFER_SRC → TRANSFER_DST.
        barrier_to_layout(
            device,
            cb,
            ca.src_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
        );
        // Copy scratch → src (== dst) at dst_rect.
        let region2 = [vk::ImageCopy::default()
            .src_subresource(color_layers())
            .src_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .dst_subresource(color_layers())
            .dst_offset(vk::Offset3D {
                x: ca.dst_rect.offset.x,
                y: ca.dst_rect.offset.y,
                z: 0,
            })
            .extent(vk::Extent3D {
                width: ca.dst_rect.extent.width,
                height: ca.dst_rect.extent.height,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image(
                cb,
                scratch.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                ca.src_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region2,
            );
        }
        // (4) src (== dst) → SHADER_READ_ONLY_OPTIMAL (N1 terminal-layout rule).
        barrier_to_layout(
            device,
            cb,
            ca.src_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );
        return Ok(());
    }

    // Disjoint case: two-barrier pre-sequence + copy + two-barrier post-sequence.
    // Pre-barriers: src → TRANSFER_SRC, dst → TRANSFER_DST (exact N1 masks).
    barrier_to_layout(
        device,
        cb,
        ca.src_image,
        ca.src_old_layout,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_READ,
    );
    barrier_to_layout(
        device,
        cb,
        ca.dst_image,
        ca.dst_old_layout,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
    );
    let region = [vk::ImageCopy::default()
        .src_subresource(color_layers())
        .src_offset(vk::Offset3D {
            x: ca.src_rect.offset.x,
            y: ca.src_rect.offset.y,
            z: 0,
        })
        .dst_subresource(color_layers())
        .dst_offset(vk::Offset3D {
            x: ca.dst_rect.offset.x,
            y: ca.dst_rect.offset.y,
            z: 0,
        })
        .extent(vk::Extent3D {
            width: ca.dst_rect.extent.width,
            height: ca.dst_rect.extent.height,
            depth: 1,
        })];
    unsafe {
        device.cmd_copy_image(
            cb,
            ca.src_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            ca.dst_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &region,
        );
    }
    // Post-barriers: BOTH src and dst → SHADER_READ_ONLY_OPTIMAL (N1).
    barrier_to_layout(
        device,
        cb,
        ca.src_image,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_READ,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    barrier_to_layout(
        device,
        cb,
        ca.dst_image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}

/// Phase B.3 clip — replay a deferred `RecordedMaskedCopyArea`: a masked_blit
/// graphics draw that copies `sample_view` → dst gated by `mask_view`'s R8
/// coverage. NO snapshot refresh here — the snapshot is brought up to date by a
/// separate `RecordedOp::ClipSnapshotRefresh` emitted earlier this frame.
fn emit_recorded_masked_copyarea_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    generation: u64,
    m: &crate::kms::render::frame_builder::RecordedMaskedCopyArea,
) -> Result<(), RenderError> {
    let device = inner.vk.device.clone();

    // NO refresh here: the snapshot is brought up to date by a separate
    // RecordedOp::ClipSnapshotRefresh emitted earlier this frame (Task 14). The
    // masked op only SAMPLES the snapshot, whose `mask_old_layout` is SHADER_READ.

    // (2) Self-overlap: copy the LIVE src region → scratch@(0,0), then sample
    // scratch. `dst_is_transfer_src` tracks that dst (== src) is left in
    // TRANSFER_SRC by this copy, so the (3) dst→COLOR barrier uses the right
    // old layout (codex round-4 finding 1).
    let dst_is_transfer_src = m.self_overlap_scratch.is_some();
    if let Some(scratch) = m.self_overlap_scratch.as_ref() {
        barrier_to_layout(
            &device,
            cb,
            m.src_image,
            m.src_old_layout,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );
        barrier_to_layout(
            &device,
            cb,
            scratch.image,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::PipelineStageFlags2::TOP_OF_PIPE,
            vk::AccessFlags2::empty(),
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
        );
        // src region = the clamped LIVE src rect (live_src_offset); scratch holds
        // it at (0,0). NOTE: do NOT use copy_offset here — it is the rewritten
        // sample-space offset (−dst_rect.offset) on this path (finding 1).
        let region = [vk::ImageCopy::default()
            .src_subresource(color_layers())
            .src_offset(vk::Offset3D {
                x: m.live_src_offset[0],
                y: m.live_src_offset[1],
                z: 0,
            })
            .dst_subresource(color_layers())
            .dst_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(vk::Extent3D {
                width: m.dst_rect.extent.width,
                height: m.dst_rect.extent.height,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image(
                cb,
                m.src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                scratch.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region,
            );
        }
        barrier_to_layout(
            &device,
            cb,
            scratch.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );
        // NOTE: the COPY reads `m.src_image` (the LIVE drawable, == dst here).
        // The DRAW samples `m.sample_view` (= scratch.view, set in Task 7), and
        // `m.copy_offset` is rewritten so src_texel = dst_pixel - dst_rect.offset.
    } else {
        barrier_to_layout(
            &device,
            cb,
            m.src_image,
            m.src_old_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );
    }

    // Mask → SHADER_READ_ONLY. `mask_old_layout` is SHADER_READ when the snapshot
    // was just refreshed this frame, but may be UNDEFINED/other for the Phase-1
    // plain-drawable test path — so always emit the transition. A no-op SHADER_READ
    // → SHADER_READ barrier still provides the execution/memory dependency that
    // orders this draw after a same-frame ClipSnapshotRefresh write to the snapshot.
    barrier_to_layout(
        &device,
        cb,
        m.mask_image,
        m.mask_old_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );

    // (3) dst → COLOR_ATTACHMENT. On self-overlap, dst (== src) was left in
    // TRANSFER_SRC by the (2) copy, so the old layout + producer stage/access
    // differ from the non-overlap case (codex round-4 finding 1).
    let (dst_old, dst_src_stage, dst_src_access) = if dst_is_transfer_src {
        (
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        )
    } else {
        (
            m.dst_old_layout,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        )
    };
    barrier_to_layout(
        &device,
        cb,
        m.dst_image,
        dst_old,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        dst_src_stage,
        dst_src_access,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE | vk::AccessFlags2::COLOR_ATTACHMENT_READ,
    );

    // (4) pipeline + descriptor set.
    let mb = inner
        .masked_blit
        .as_mut()
        .ok_or(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
    let pipeline = mb.pipeline_for(m.dst_format).map_err(RenderError::Vk)?;
    let pipeline_layout = mb.pipeline_layout;
    let dsl = mb.descriptor_set_layout;
    let set = inner
        .descriptor_pool_ring
        .acquire_set(dsl, generation)
        .map_err(RenderError::Vk)?;
    inner
        .masked_blit
        .as_ref()
        .expect("masked_blit present")
        // Bind the SAMPLED view (src identity view, or scratch view on
        // self-overlap) — NOT the live src image (codex round-4 finding 2).
        .write_views(set, m.sample_view, m.mask_view);

    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: m.dst_extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(m.dst_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: m.dst_extent.width as f32,
        height: m.dst_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    unsafe {
        device.cmd_begin_rendering(cb, &rendering_info);
        device.cmd_set_viewport(cb, 0, &viewport);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::GRAPHICS,
            pipeline_layout,
            0,
            &[set],
            &[],
        );
        let pc = crate::kms::vk::masked_blit_pipeline::MaskedBlitPushConsts {
            dst_origin: [m.dst_rect.offset.x as f32, m.dst_rect.offset.y as f32],
            dst_size: [
                m.dst_rect.extent.width as f32,
                m.dst_rect.extent.height as f32,
            ],
            viewport: [m.dst_extent.width as f32, m.dst_extent.height as f32],
            copy_offset: m.copy_offset,
            clip_offset: m.clip_origin, // frag: mask_texel = dst_pixel - clip_offset
            // OOB check is against the SAMPLED image (src, or scratch on
            // self-overlap), so push sample_extent (codex round-4 finding 2).
            src_extent: [m.sample_extent.width as i32, m.sample_extent.height as i32],
            mask_extent: [m.mask_extent.width as i32, m.mask_extent.height as i32],
        };
        for s in &m.scissors {
            let sc = [*s];
            device.cmd_set_scissor(cb, 0, &sc);
            device.cmd_push_constants(
                cb,
                pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                pc.as_bytes(),
            );
            device.cmd_draw(cb, 4, 1, 0, 0);
        }
        device.cmd_end_rendering(cb);
    }

    // (5) dst → SHADER_READ_ONLY (N1 terminal layout).
    barrier_to_layout(
        &device,
        cb,
        m.dst_image,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}

/// Standalone snapshot refresh: cmd_copy_image live clip pixmap → GC-owned
/// snapshot, leaving BOTH at SHADER_READ_ONLY_OPTIMAL (N1). The `ALL_COMMANDS`
/// source stage on the live read orders this after any same-frame write to the
/// live mask; the snapshot→SHADER_READ barrier orders a later masked-blit's
/// sample after this copy (the masked op records mask_old_layout = SHADER_READ).
fn emit_recorded_clip_snapshot_refresh_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    r: &crate::kms::render::frame_builder::RecordedClipSnapshotRefresh,
) -> Result<(), RenderError> {
    let device = inner.vk.device.clone();
    // live → TRANSFER_SRC.
    barrier_to_layout(
        &device,
        cb,
        r.live_mask_image,
        r.live_mask_old_layout,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_READ,
    );
    // snapshot → TRANSFER_DST.
    barrier_to_layout(
        &device,
        cb,
        r.snapshot_image,
        r.snapshot_old_layout,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
    );
    let region = [vk::ImageCopy::default()
        .src_subresource(color_layers())
        .dst_subresource(color_layers())
        .extent(vk::Extent3D {
            width: r.copy_extent.width,
            height: r.copy_extent.height,
            depth: 1,
        })];
    unsafe {
        device.cmd_copy_image(
            cb,
            r.live_mask_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            r.snapshot_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &region,
        );
    }
    // snapshot → SHADER_READ (a later masked-blit samples it).
    barrier_to_layout(
        &device,
        cb,
        r.snapshot_image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    // live → SHADER_READ (N1 terminal for the live mask drawable).
    barrier_to_layout(
        &device,
        cb,
        r.live_mask_image,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_READ,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}

/// Phase B.3 Task 6 (N1 + N2): replay a deferred `RecordedPutImage` into
/// the frame's command buffer. Staging buffer handle is read from the
/// frame pin-set (N2 — index pre-recorded at append time). Barrier shape
/// mirrors the legacy `put_image` body (engine.rs pre-B.3 lines ~3684-3731):
///
/// - Pre-barrier: dst `old_layout` → `TRANSFER_DST_OPTIMAL` with
///   `ALL_COMMANDS / SHADER_SAMPLED_READ | COLOR_ATTACHMENT_WRITE`
///   producer mask (N1 — drains prior compose/fill/paint writes).
/// - `cmd_copy_buffer_to_image` from the pinned staging buffer.
/// - Post-barrier: dst `TRANSFER_DST_OPTIMAL` → `SHADER_READ_ONLY_OPTIMAL`
///   (N1 terminal layout).
fn emit_recorded_put_image_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    pins: &crate::kms::render::frame_builder::FramePinSet,
    pi: &crate::kms::render::frame_builder::RecordedPutImage,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    // N1 put_image pre-barrier (DST only — staging buffers have no layout).
    // Mirrors engine.rs:3684-3692 producer mask: SHADER_SAMPLED_READ |
    // COLOR_ATTACHMENT_WRITE drains any prior compose/fill/put-image writes
    // to this drawable.
    barrier_to_layout(
        device,
        cb,
        pi.dst_image,
        pi.dst_old_layout,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
    );
    // N2: read the staging buffer handle from the frame pin-set.
    let staging_buffer = pins.staging_buffers[pi.staging_pin_idx.0 as usize].buffer;
    let region = [vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(
            vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .layer_count(1),
        )
        .image_offset(vk::Offset3D {
            x: pi.dst_rect.offset.x,
            y: pi.dst_rect.offset.y,
            z: 0,
        })
        .image_extent(vk::Extent3D {
            width: pi.dst_rect.extent.width,
            height: pi.dst_rect.extent.height,
            depth: 1,
        })];
    unsafe {
        device.cmd_copy_buffer_to_image(
            cb,
            staging_buffer,
            pi.dst_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &region,
        );
    }
    // N1 post-barrier: dst → SHADER_READ_ONLY_OPTIMAL (terminal layout).
    // Mirrors engine.rs:3723-3731.
    barrier_to_layout(
        device,
        cb,
        pi.dst_image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}

/// Open a dynamic-rendering color pass on `dst`: pre-barrier from
/// `old_layout` → COLOR_ATTACHMENT_OPTIMAL with the caller's producer
/// `src_access` mask (kept per-kind — fill/logic pass the superset), then
/// `cmd_begin_rendering` (LOAD/STORE, full-extent render area) + viewport.
/// Does NOT bind a pipeline or scissor — those are per-op (draws half).
/// Emits the SAME rendering commands+order as the fill/logic open
/// prologues — the only difference is that this counts
/// `begin_rendering`/`set_viewport` via `vk_count!`, which the inline
/// fill/logic code does NOT today (telemetry fix, see Phase 1 header).
/// It is NOT a drop-in for composite's open (`render.rs`), which also
/// binds the pipeline + counts it; composite keeps using its own
/// `render.rs` open in Phase 1.
fn open_dst_color_pass(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    dst_image: vk::Image,
    dst_view: vk::ImageView,
    dst_extent: vk::Extent2D,
    old_layout: vk::ImageLayout,
    src_access: vk::AccessFlags2,
) {
    barrier_to_layout(
        &vk.device,
        cb,
        dst_image,
        old_layout,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        src_access,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
    );
    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: dst_extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(dst_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);
    #[allow(clippy::cast_precision_loss)]
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: dst_extent.width as f32,
        height: dst_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    unsafe {
        crate::vk_count!(cmd_begin_rendering);
        vk.device.cmd_begin_rendering(cb, &rendering_info);
        crate::vk_count!(cmd_set_viewport);
        vk.device.cmd_set_viewport(cb, 0, &viewport);
    }
}

/// Close a pass opened by `open_dst_color_pass`: `cmd_end_rendering` +
/// post-barrier COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL.
/// Emits exactly the commands the per-kind close halves emit today.
pub(super) fn close_dst_color_pass(vk: &VkContext, cb: vk::CommandBuffer, dst_image: vk::Image) {
    unsafe {
        crate::vk_count!(cmd_end_rendering);
        vk.device.cmd_end_rendering(cb);
    }
    barrier_to_layout(
        &vk.device,
        cb,
        dst_image,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
}

/// Phase B.3 Task 8: replay a deferred `RecordedFillRect` into the
/// frame's command buffer. Uses `cmd_clear_attachments` directly —
/// NO composite pipeline, NO descriptor (codex round-7 catch —
/// earlier drafts erroneously routed through composite).
///
/// `load_op = LOAD` is LOAD-BEARING per N4: outside-rect pixels must
/// be preserved. `DONT_CARE` would invalidate the entire render area.
///
/// Pre-barrier producer mask mirrors the legacy path at the old
/// engine.rs fill_rect_batch body: `ALL_COMMANDS /
/// SHADER_SAMPLED_READ | TRANSFER_WRITE | COLOR_ATTACHMENT_WRITE`
/// drains any prior compose reads / put_image writes / fill writes
/// on the same image.
fn emit_recorded_fill_rect_into_cb(
    inner: &mut RenderEngineInner,
    store: &DrawableStore,
    cb: vk::CommandBuffer,
    fr: &crate::kms::render::frame_builder::RecordedFillRect,
) -> Result<(), RenderError> {
    // Clone the Vk handle owner so the helper calls don't alias
    // `&inner.vk` against `&mut inner`.
    let vk = inner.vk.clone();
    // Resolve the dst vk::Image at emit time — the payload carries
    // dst_id so we can look it up from the store. The storage image is
    // stable for the drawable's lifetime; no invalidation risk.
    let dst_image = store
        .get(fr.dst_id)
        .ok_or(RenderError::UnknownDrawable(fr.dst_id))?
        .storage
        .image;

    // FILL pre-barrier + begin_rendering + viewport via the shared open
    // helper. Producer mask is the legacy fill superset (ALL_COMMANDS /
    // SHADER_SAMPLED_READ | TRANSFER_WRITE | COLOR_ATTACHMENT_WRITE) —
    // NOT unified with composite's narrow mask in Phase 1.
    open_dst_color_pass(
        &vk,
        cb,
        dst_image,
        fr.dst_image_view,
        fr.dst_extent,
        fr.dst_old_layout,
        SESSION_SRC_ACCESS,
    );
    // Draws half (UNCHANGED): scissor to render_area + clear_attachments.
    emit_fill_draws(&vk, cb, fr);
    // end_rendering + post-barrier (→ SHADER_READ_ONLY_OPTIMAL) via the
    // shared close helper.
    close_dst_color_pass(&vk, cb, dst_image);
    Ok(())
}

/// Producer access mask for EVERY session open pre-barrier (fill / logic_fill
/// / fold-clean composite) — the superset that drains prior compose reads /
/// put_image writes / fill writes on the same image. Phase 3 unifies this
/// across all session-opener kinds (was fill-specific): cross-kind batching is
/// intentionally on, so the open barrier must conservatively cover composite
/// AND fill producers regardless of which kind opens the session. The
/// STANDALONE composite path keeps its own narrow `SHADER_SAMPLED_READ` mask
/// (in `record_render_composite_open_with_old_layout`), unchanged.
const SESSION_SRC_ACCESS: vk::AccessFlags2 = vk::AccessFlags2::from_raw(
    vk::AccessFlags2::SHADER_SAMPLED_READ.as_raw()
        | vk::AccessFlags2::TRANSFER_WRITE.as_raw()
        | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE.as_raw(),
);

/// FILL draws-half: assumes a pass is OPEN (via `open_dst_color_pass`).
/// Sets the scissor to the full render area, then `cmd_clear_attachments`
/// for every recorded rect. NO open/close — the session (or the standalone
/// wrapper) owns those. Re-set scissor on every call so a session
/// `Continue` is correct after a prior op left a different scissor bound.
fn emit_fill_draws(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    fr: &crate::kms::render::frame_builder::RecordedFillRect,
) {
    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: fr.dst_extent,
    };
    let attachments = [vk::ClearAttachment::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .color_attachment(0)
        .clear_value(vk::ClearValue {
            color: vk::ClearColorValue { float32: fr.color },
        })];
    let clear_rects: Vec<vk::ClearRect> = fr
        .rects
        .iter()
        .map(|r| {
            vk::ClearRect::default()
                .rect(*r)
                .base_array_layer(0)
                .layer_count(1)
        })
        .collect();
    unsafe {
        let scissor = [render_area];
        vk.device.cmd_set_scissor(cb, 0, &scissor);
        vk.device
            .cmd_clear_attachments(cb, &attachments, &clear_rects);
    }
}

/// Phase B.3 Task 10: replay a `RecordedLogicFill` into the frame CB.
///
/// Mirrors engine.rs pre-B.3 `logic_fill` emit body (lines ~2593-2697):
/// - pipeline re-resolved FRESH via `inner.logic_fill_caches[dst_format]
///   .get(logic_mode, opaque_alpha)` (cache is engine-owned + stable per N6).
/// - Single `cmd_set_viewport` OUTSIDE the per-rect loop (N6 invariant).
/// - Push constants match legacy shape: dst_origin, dst_size, viewport,
///   _pad, fg_color.
fn emit_recorded_logic_fill_into_cb(
    inner: &mut RenderEngineInner,
    store: &DrawableStore,
    cb: vk::CommandBuffer,
    lf: &crate::kms::render::frame_builder::RecordedLogicFill,
) -> Result<(), RenderError> {
    // Clone the Vk handle owner so the helper calls don't alias
    // `&inner.vk` against the `&mut inner.logic_fill_caches` borrow.
    let vk = inner.vk.clone();
    let dst_image = store
        .get(lf.dst_id)
        .ok_or(RenderError::UnknownDrawable(lf.dst_id))?
        .storage
        .image;
    let cache = inner
        .logic_fill_caches
        .get_mut(&lf.dst_format)
        .ok_or(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
    let pipeline = cache
        .get(lf.logic_mode, lf.channels)
        .map_err(|_| RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
    let pipeline_layout = cache.pipeline_layout();

    // N6 pre-barrier + begin_rendering + viewport via the shared open
    // helper. Producer mask is the legacy logic_fill superset
    // (ALL_COMMANDS / SHADER_SAMPLED_READ | TRANSFER_WRITE |
    // COLOR_ATTACHMENT_WRITE) — NOT unified with composite in Phase 1.
    // The helper sets the viewport ONCE before the draws half — same
    // position as the legacy single cmd_set_viewport before the per-rect
    // loop (N6 invariant).
    open_dst_color_pass(
        &vk,
        cb,
        dst_image,
        lf.dst_image_view,
        lf.dst_extent,
        lf.dst_old_layout,
        SESSION_SRC_ACCESS,
    );
    // Draws half (UNCHANGED, minus the viewport now set by the helper):
    // bind_pipeline then per-rect scissor/push/draw.
    emit_logic_fill_draws(&vk, cb, pipeline, pipeline_layout, lf);
    // end_rendering + post-barrier (→ SHADER_READ_ONLY_OPTIMAL) via the
    // shared close helper.
    close_dst_color_pass(&vk, cb, dst_image);
    Ok(())
}

/// LOGIC_FILL draws-half: assumes a pass is OPEN and the caller resolved
/// the pipeline + layout from `inner.logic_fill_caches[dst_format]`.
/// Re-binds the pipeline (dynamic rendering allows mid-pass rebind) and
/// re-sets the scissor per rect, so a session `Continue` is correct after
/// any prior op's pipeline/scissor state. NO open/close.
fn emit_logic_fill_draws(
    vk: &VkContext,
    cb: vk::CommandBuffer,
    pipeline: vk::Pipeline,
    pipeline_layout: vk::PipelineLayout,
    lf: &crate::kms::render::frame_builder::RecordedLogicFill,
) {
    use crate::kms::vk::logic_fill_pipeline::LogicFillPushConsts;
    #[allow(clippy::cast_precision_loss)]
    let dst_vp = [lf.dst_extent.width as f32, lf.dst_extent.height as f32];
    unsafe {
        vk.device
            .cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
        for r in &lf.rects {
            let scissor = [*r];
            vk.device.cmd_set_scissor(cb, 0, &scissor);
            #[allow(clippy::cast_precision_loss)]
            let pc = LogicFillPushConsts {
                dst_origin: [r.offset.x as f32, r.offset.y as f32],
                dst_size: [r.extent.width as f32, r.extent.height as f32],
                viewport: dst_vp,
                _pad: [0.0, 0.0],
                fg_color: lf.color,
            };
            vk.device.cmd_push_constants(
                cb,
                pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                pc.as_bytes(),
            );
            vk.device.cmd_draw(cb, 4, 1, 0, 0);
        }
    }
}

/// Slice-2 phase-3 COMPOSITE draws-half: assumes a pass is already OPEN (via
/// `open_dst_color_pass`, which does NOT bind a pipeline). For a FOLD-CLEAN
/// composite this does steps (3) pipeline lookup + bind_pipeline + (5) clip
/// scissors build + `record_render_composite_draws` of
/// `emit_recorded_render_composite_into_cb`, but NONE of the pre-pass
/// steps (1)(2)(2b) (solid clears / src-alias / dst-readback copies — illegal
/// mid-pass) and NEITHER the (4) open NOR the (6) close. `folder_clean`
/// guarantees there is no pre-pass work and no dst self-read (asserted below).
///
/// The empty-picture-clip case (`Some([])` → no scissors) skips the draw with
/// an early `return Ok(())` — it must NOT close the session; the session close
/// happens later in the replay loop on a hazard / end-of-frame.
fn emit_composite_draws(
    inner: &mut RenderEngineInner,
    vk: &VkContext,
    cb: vk::CommandBuffer,
    rc: &crate::kms::render::frame_builder::RecordedRenderComposite,
) -> Result<(), RenderError> {
    use crate::kms::vk::{ops::render as vk_render, render_pipeline::StdPictOp};

    // folder_clean invariant (== eligibility gate): no pre-pass transfer and
    // no dst self-read, so it is safe to draw mid-session.
    debug_assert!(
        rc.src_clear_color.is_none()
            && rc.mask_clear_color.is_none()
            && rc.src_alias_view.is_none()
            && !rc.needs_dst_readback
            && rc.src_view != rc.dst_view
            && rc.mask_view != rc.dst_view,
        "emit_composite_draws requires a fold-clean composite (no pre-pass, no dst self-read)"
    );

    // (3) Pipeline lookup. The cache `get` takes `&mut self`; resolve the
    // pipeline + layout and RELEASE the borrow before drawing with `vk`.
    let std_op = StdPictOp::from_u8(rc.op).expect("op validated at append in via_frame_builder");
    let (pipeline, pipeline_layout) = {
        let cache = inner
            .render_pipelines
            .as_mut()
            .expect("render_pipelines: ensured at op-append");
        let pipeline = cache
            .get(
                std_op,
                rc.dst_format,
                rc.dst_has_alpha,
                rc.mask_component_alpha,
            )
            .map_err(|e| {
                log::warn!("emit_composite_draws: pipeline get failed: {e:?}");
                RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
            })?;
        let pipeline_layout = cache.pipeline_layout();
        (pipeline, pipeline_layout)
    };

    // `open_dst_color_pass` does NOT bind a pipeline (unlike composite's
    // standalone open), so bind it here. Dynamic rendering allows a mid-pass
    // pipeline rebind, so a session `Continue` is correct after any prior op.
    unsafe {
        crate::vk_count!(cmd_bind_pipeline);
        vk.device
            .cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
    }

    // (5) Per-rect draws. Same Some(cr)/None/empty-clip logic as the
    // standalone path, but the empty-clip case returns WITHOUT closing the
    // session (the loop closes it later).
    let full_extent_scissor;
    // #133 step 3 (P4): the recorded content bounds stand in for the
    // storage extent, so a deferred composite is scissored to the
    // drawable's content even with no picture clip of its own.
    let dst_bounds = resolve_recorded_bounds(rc.dst_bounds, rc.dst_extent);
    let clip_scissors: &[vk::Rect2D] = match rc.clip_rects.as_deref() {
        Some(cr) => {
            // `None` => no picture clip, paint everywhere.
            // `Some([])` => empty picture clip, paint nothing.
            let owned = build_render_clip_scissors_to(Some(cr), dst_bounds);
            if owned.is_empty() {
                // Empty clip: skip the draw. Do NOT close the session.
                return Ok(());
            }
            full_extent_scissor = owned;
            full_extent_scissor.as_slice()
        }
        None => {
            full_extent_scissor = vec![dst_bounds];
            full_extent_scissor.as_slice()
        }
    };
    vk_render::record_render_composite_draws(
        vk,
        cb,
        pipeline_layout,
        rc.descriptor_set,
        rc.dst_extent,
        &rc.attrs,
        &rc.rects,
        clip_scissors,
    );
    Ok(())
}

/// Slice-2: open a new session pass for an eligible (fill / logic_fill /
/// fold-clean composite) op using THIS op's recorded `dst_old_layout` /
/// image / view / extent, then emit its draws-half. Sets `*session` to the
/// new open pass. The opener's own `dst_old_layout` is the overlay-resolved
/// layout before the group; the post-barrier to SHADER_READ is deferred to
/// `close_dst_color_pass`. Only ever called for `RecordedOp::FillRect` /
/// `RecordedOp::LogicFill` / fold-clean `RecordedOp::RenderComposite` (the
/// `session_eligible` gate guarantees it).
pub(super) fn emit_session_open_and_draws(
    inner: &mut RenderEngineInner,
    store: &DrawableStore,
    vk: &VkContext,
    cb: vk::CommandBuffer,
    op: &crate::kms::render::frame_builder::RecordedOp,
    session: &mut Option<DstPassSession>,
) -> Result<(), RenderError> {
    use crate::kms::render::frame_builder::RecordedOp as Op;
    match op {
        Op::FillRect(fr) => {
            let dst_image = store
                .get(fr.dst_id)
                .ok_or(RenderError::UnknownDrawable(fr.dst_id))?
                .storage
                .image;
            open_dst_color_pass(
                vk,
                cb,
                dst_image,
                fr.dst_image_view,
                fr.dst_extent,
                fr.dst_old_layout,
                SESSION_SRC_ACCESS,
            );
            emit_fill_draws(vk, cb, fr);
            *session = Some(DstPassSession {
                dst_id: fr.dst_id,
                dst_image,
                dst_view: fr.dst_image_view,
                dst_extent: fr.dst_extent,
            });
            Ok(())
        }
        Op::LogicFill(lf) => {
            let dst_image = store
                .get(lf.dst_id)
                .ok_or(RenderError::UnknownDrawable(lf.dst_id))?
                .storage
                .image;
            let cache = inner
                .logic_fill_caches
                .get_mut(&lf.dst_format)
                .ok_or(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
            let pipeline = cache
                .get(lf.logic_mode, lf.channels)
                .map_err(|_| RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
            let pipeline_layout = cache.pipeline_layout();
            open_dst_color_pass(
                vk,
                cb,
                dst_image,
                lf.dst_image_view,
                lf.dst_extent,
                lf.dst_old_layout,
                SESSION_SRC_ACCESS,
            );
            emit_logic_fill_draws(vk, cb, pipeline, pipeline_layout, lf);
            *session = Some(DstPassSession {
                dst_id: lf.dst_id,
                dst_image,
                dst_view: lf.dst_image_view,
                dst_extent: lf.dst_extent,
            });
            Ok(())
        }
        Op::RenderComposite(rc) => {
            // Fold-clean composite (eligibility-gated): no pre-pass work, so
            // open directly with the op's recorded dst_old_layout + the
            // unified session producer mask, then emit the composite draws.
            // Prefer the store-resolved image (matches fill/logic); it equals
            // the recorded `rc.dst_image` the standalone path uses.
            let dst_image = store
                .get(rc.dst_id)
                .map_or(rc.dst_image, |d| d.storage.image);
            open_dst_color_pass(
                vk,
                cb,
                dst_image,
                rc.dst_view,
                rc.dst_extent,
                rc.dst_old_layout,
                SESSION_SRC_ACCESS,
            );
            emit_composite_draws(inner, vk, cb, rc)?;
            *session = Some(DstPassSession {
                dst_id: rc.dst_id,
                dst_image,
                dst_view: rc.dst_view,
                dst_extent: rc.dst_extent,
            });
            Ok(())
        }
        // session_eligible only returns Some for FillRect / LogicFill /
        // fold-clean RenderComposite, so the loop never routes another kind
        // here.
        _ => unreachable!("emit_session_open_and_draws called for ineligible op kind"),
    }
}

/// Slice-2: emit ONLY the draws-half of an eligible op into the already-open
/// session pass (no open/close, no barrier). Same eligibility guarantee as
/// `emit_session_open_and_draws`. The fill draws-half re-sets the scissor;
/// the logic draws-half re-binds the pipeline + re-sets per-rect scissor.
pub(super) fn emit_session_continue_draws(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    op: &crate::kms::render::frame_builder::RecordedOp,
) -> Result<(), RenderError> {
    use crate::kms::render::frame_builder::RecordedOp as Op;
    // Clone the Vk handle owner so the draws call doesn't alias `&inner.vk`
    // against `&mut inner.logic_fill_caches` (logic path).
    let vk = inner.vk.clone();
    match op {
        Op::FillRect(fr) => {
            emit_fill_draws(&vk, cb, fr);
            Ok(())
        }
        Op::LogicFill(lf) => {
            let cache = inner
                .logic_fill_caches
                .get_mut(&lf.dst_format)
                .ok_or(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
            let pipeline = cache
                .get(lf.logic_mode, lf.channels)
                .map_err(|_| RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
            let pipeline_layout = cache.pipeline_layout();
            emit_logic_fill_draws(&vk, cb, pipeline, pipeline_layout, lf);
            Ok(())
        }
        Op::RenderComposite(rc) => {
            // Continue into the open pass: bind pipeline + descriptor + draw,
            // NO open/close/barrier. Fold-clean guaranteed by eligibility.
            emit_composite_draws(inner, &vk, cb, rc)
        }
        _ => unreachable!("emit_session_continue_draws called for ineligible op kind"),
    }
}

/// Phase B.3 Task 14 (N7): replay a deferred `RecordedImageText`
/// into the frame's command buffer. Mirrors the legacy
/// `record_text_run` call shape from the pre-B.3 `image_text` body
/// (engine.rs:4049-4070). The key difference from B.1's
/// `emit_recorded_op_into_cb`'s `CompositeGlyphs` arm:
/// - Uses `record_text_run` (single-run, NO clip scissors), NOT
///   `record_text_run_scissored` (which carries an X RENDER picture clip).
/// - Carries `dst_old_layout` from the recorded payload instead of the
///   live storage layout (Pitfall 5 — the live layout is stale during
///   deferred emit).
fn emit_recorded_image_text_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    pins: &crate::kms::render::frame_builder::FramePinSet,
    it: &crate::kms::render::frame_builder::RecordedImageText,
) -> Result<(), RenderError> {
    let atlas_extent = inner
        .glyph_atlas
        .as_ref()
        .ok_or(RenderError::NoVk)?
        .extent();
    // Clone the Vk handle so the recorder call doesn't alias
    // the pipeline cache against `&inner.vk`.
    let vk = inner.vk.clone();
    // Per-glyph instance vertex buffer pinned at record time (#1).
    let instance = pins.upload_slices[it.instance_pin.0 as usize];
    let drawable = store
        .get_mut(it.dst_id)
        .ok_or(RenderError::UnknownDrawable(it.dst_id))?;
    // Build the StorageTextTarget adapter using the recorded
    // `dst_old_layout` (Pitfall 5 — the drawable's live
    // `current_layout` is stale during deferred emit; the overlay
    // has already been committed by push_op_and_set_layouts).
    let drawable_extent = drawable.storage.extent;
    let mut adapter = StorageTextTarget {
        extent: drawable_extent,
        image: drawable.storage.image,
        image_view: drawable.storage.image_view,
        current_layout: it.dst_old_layout,
    };
    // Core ImageText is always Over+BGRA8 — the legacy singleton
    // entry, built at record time by `ensure_text_pipeline`.
    let pipeline = inner
        .text_pipelines
        // `false` — core text is never component-alpha (design
        // invariant 8).
        .get(&(3, vk::Format::B8G8R8A8_UNORM, true, false))
        .ok_or(RenderError::NoVk)?;
    // image_text uses single-run record_text_run (no clip scissors),
    // distinct from composite_glyphs's record_text_run_scissored.
    //
    // #133 step 3 (P4): a bordered destination is the one case where
    // core text DOES need a scissor — the content rect — and the
    // scissored recorder is the same draw with `cmd_set_scissor` per
    // rect. `dst_bounds == None` (every pixmap, every bw == 0 window)
    // keeps the unscissored call verbatim.
    if let Some(bounds) = it.dst_bounds {
        crate::kms::vk::ops::text::record_text_run_scissored(
            &vk,
            cb,
            &mut adapter,
            atlas_extent,
            pipeline,
            instance.buffer,
            instance.offset,
            // Core text records one run per call; no split, no offset.
            0,
            it.instance_count,
            it.foreground_rgba,
            &[clamp_rect(bounds, drawable_extent)],
        )?;
    } else {
        crate::kms::vk::ops::text::record_text_run(
            &vk,
            cb,
            &mut adapter,
            atlas_extent,
            pipeline,
            instance.buffer,
            instance.offset,
            it.instance_count,
            it.foreground_rgba,
        )?;
    }
    // Propagate the adapter's tracked layout back into the drawable's
    // storage — record_text_run transitions to SHADER_READ_ONLY_OPTIMAL.
    drawable.storage.current_layout = adapter.current_layout;
    Ok(())
}

/// Phase B.3 Task 12 (N5): replay a deferred `RecordedRenderTrapsOrTris`
/// into the frame's command buffer. Mirrors the legacy
/// `render_traps_or_tris` two-stage CB (raster phase + composite phase).
///
/// All four resources NOT recorded per N5 are re-resolved FRESH:
/// - `engine.mask_scratch` (image, attachment_view, image_view, extent, current_layout).
/// - `engine.dst_readback` view when `std_op.needs_dst_readback()`.
/// - composite pipeline via `render_pipelines.get(std_op, …)`.
/// - descriptor set via `allocate_descriptor_for_views_into_ring` using
///   `open_frame.frame_generation` as the watermark.
///
/// Post-emit CPU writeback (N5 LOAD-BEARING, codex round-10):
/// `inner.mask_scratch.set_current_layout(SHADER_READ_ONLY_OPTIMAL)` after
/// the composite-close barrier — without this the NEXT trap op's pre-barrier
/// reads a stale old_layout (VUID-class bug).
/// Per-axis source sampling origin for the RENDER `Trapezoids`/
/// `Triangles` composite stage. `base` is the client `xSrc`/`ySrc`
/// already shifted by the caller's dst redirect / `x_off` delta. When
/// the op renders over the full dst (`needs_full_dst`) the coverage
/// mask carries the bbox offset, so the source aligns directly at
/// `base`; otherwise the composite renders at the bbox origin and the
/// source must add it back. Mirrors Xorg `miTrapezoids`, where the
/// source is sampled at `xSrc + dst_px`.
#[inline]
pub(super) fn trap_composite_src_origin_axis(base: i32, bbox: i32, needs_full_dst: bool) -> i32 {
    if needs_full_dst { base } else { base + bbox }
}

#[allow(clippy::too_many_arguments)]
fn emit_recorded_render_traps_or_tris_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    pins: &crate::kms::render::frame_builder::FramePinSet,
    frame_generation: u64,
    rt: &crate::kms::render::frame_builder::RecordedRenderTrapsOrTris,
) -> Result<(), RenderError> {
    use crate::kms::vk::{
        ops::render as vk_render, render_pipeline::record_solid_color_clear,
        trap_pipeline::TrapDrawPushConsts,
    };

    // ── (a) Resolve src view FRESH from engine caches at emit time ──
    let solid_src_view = inner
        .solid_src_image
        .as_ref()
        .expect("solid_src_image: ensure_render_assets ran at append")
        .image_view();

    let src_view = match &rt.src_kind {
        crate::kms::render::frame_builder::RecordedTrapSrcKind::Drawable {
            id,
            swizzle_class,
            sample_offset: _,
        } => {
            let info =
                drawable_for_render_view(store, *id).ok_or(RenderError::UnknownDrawable(*id))?;
            // Use the snapshot swizzle_class (append-time stable, per N5).
            // Mirror the non-deferred composite paths: derive the src
            // view's sampler from the picture's repeat mode (REPEAT_NONE
            // → clamp-to-border, not the previously hardcoded
            // clamp-to-edge). The in-shader `apply_repeat` already zeroes
            // out-of-bounds samples for REPEAT_NONE, so this is mostly
            // hygiene/consistency — but it removes a latent edge-texel
            // leak at the exact-`uv==1.0` boundary.
            let sampler = sampler_config_for_shader_repeat(rt.src_repeat);
            ensure_drawable_view(
                &inner.vk,
                &mut inner.drawable_view_cache,
                *id,
                info.image,
                info.format,
                sampler,
                *swizzle_class,
            )?
        }
        crate::kms::render::frame_builder::RecordedTrapSrcKind::Solid(color) => {
            // record_solid_color_clear writes the colour into the 1×1 scratch
            // BEFORE the trap raster phase; the view is the solid_src_view.
            let solid = inner
                .solid_src_image
                .as_mut()
                .expect("solid_src_image: ensure_render_assets ran at append");
            record_solid_color_clear(&inner.vk, cb, solid, *color);
            solid_src_view
        }
        crate::kms::render::frame_builder::RecordedTrapSrcKind::Gradient {
            picture,
            intrinsic_axis_projection: _,
        } => {
            // B.3 hotfix 2: the Arc clone guarantees liveness — no
            // picture_paint lookup, no None branch. The "missing at
            // emit" warn path is gone.
            picture.image_view()
        }
    };

    // ── (b) Resolve mask_scratch FRESH ──
    let mask_scratch = inner
        .mask_scratch
        .as_ref()
        .expect("mask_scratch: ensure_trap_assets ran at append");
    let mask_image = mask_scratch.image();
    let mask_attachment_view = mask_scratch.attachment_view();
    let mask_view = mask_scratch.image_view();
    let mask_extent = mask_scratch.extent();
    let mask_src_layout = mask_scratch.current_layout();

    // ── (c) dst_readback view FRESH when std_op.needs_dst_readback() ──
    let white_mask_view = inner
        .white_mask_image
        .as_ref()
        .expect("white_mask_image: ensure_render_assets ran at append")
        .image_view();
    let dst_readback_view = if rt.std_op.needs_dst_readback() {
        let rb = inner
            .dst_readback
            .as_mut()
            .expect("dst_readback: ensured at append when needs_dst_readback");
        match rb.view(rt.dst_format, rt.dst_has_alpha) {
            Ok(Some(v)) => v,
            Ok(None) | Err(_) => {
                log::warn!(
                    "emit_recorded_render_traps_or_tris: dst_readback view unavailable — skipping"
                );
                return Ok(());
            }
        }
    } else {
        white_mask_view
    };

    // ── Resolve composite pipeline FRESH ──
    let pipeline = inner
        .render_pipelines
        .as_mut()
        .expect("render_pipelines: ensured at append")
        .get(rt.std_op, rt.dst_format, rt.dst_has_alpha, false)
        .map_err(|e| {
            log::warn!("emit_recorded_render_traps_or_tris: pipeline build {e:?}");
            RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
        })?;
    let pipeline_layout = inner
        .render_pipelines
        .as_ref()
        .expect("render_pipelines: ensured at append")
        .pipeline_layout();

    // Allocate descriptor set FRESH via B.2 Mechanism 2 watermark.
    // `frame_generation` is threaded from the close path's local copy of
    // `open_frame.frame_generation` — inner.frame_builder.open is None by
    // the time the emit dispatch loop runs (take_open_for_close clears it).
    let descriptor_set = inner
        .render_pipelines
        .as_ref()
        .expect("render_pipelines: ensured at append")
        .allocate_descriptor_for_views_into_ring(
            &mut inner.descriptor_pool_ring,
            frame_generation,
            src_view,
            mask_view,
            dst_readback_view,
        )?;

    let device = &inner.vk.device;

    // ── (d) Trap raster phase — mirror engine.rs:7531-7647 ──
    let (prim_pipeline, prim_layout) = {
        let tp = inner
            .trap_pipeline
            .as_ref()
            .expect("trap_pipeline: ensured at append");
        let pipe = match rt.prim_kind {
            TrapPrimKind::Trapezoid => tp.trapezoid_pipeline(),
            TrapPrimKind::Triangle => tp.triangle_pipeline(),
        };
        (pipe, tp.pipeline_layout())
    };

    // Barrier: mask_scratch → COLOR_ATTACHMENT_OPTIMAL.
    let (mask_src_stage, mask_src_access) = match mask_src_layout {
        vk::ImageLayout::UNDEFINED => {
            (vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::NONE)
        }
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
        _ => (
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        ),
    };
    let color_range = vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1);
    let to_attach = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(mask_src_stage)
        .src_access_mask(mask_src_access)
        .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .old_layout(mask_src_layout)
        .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .image(mask_image)
        .subresource_range(color_range)];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&to_attach);
    unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

    let bbox_render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: vk::Extent2D {
            width: rt.bbox_w,
            height: rt.bbox_h,
        },
    };
    // For ops that composite the coverage mask over the FULL dst
    // (`needs_full_dst`: op=Src and friends), the composite below samples
    // `mask_scratch` at texels OUTSIDE the trap bbox (the mask is offset
    // by `-bbox` and read across the whole destination). `mask_scratch`
    // is a persistent, power-of-two-grown, reused image (256² minimum),
    // so those out-of-bbox texels hold STALE coverage from a prior
    // trap-op. Clearing only the bbox region leaves that stale data,
    // which a full-dst composite reads as nonzero coverage → a spurious
    // alpha ridge (observed as a 2px alpha~=23 line at GTK CSD tooltip
    // box edges, where `solid_alpha(46) * stale_coverage(~50%) = 23`).
    // Clear the WHOLE scratch for these ops so out-of-bbox samples read
    // 0; the draw stays scissored to the bbox (`cmd_set_scissor` below),
    // so coverage is unchanged — only the previously-stale margin is
    // now zeroed. Non-full-dst ops render only within the bbox, never
    // sample the margin, and keep the cheaper bbox-only clear.
    let needs_full_dst_clear = matches!(
        rt.op_byte,
        0 | 1 | 5 | 6 | 7 | 10 | 13 | 16..=27 | 32..=43
    );
    let clear_render_area = if needs_full_dst_clear {
        vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: mask_extent,
        }
    } else {
        bbox_render_area
    };
    let clear = vk::ClearValue {
        color: vk::ClearColorValue {
            float32: [0.0, 0.0, 0.0, 0.0],
        },
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(mask_attachment_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .clear_value(clear)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(clear_render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);

    // Bind the vertex data pinned in the frame's upload arena (#177).
    let vertex = pins.upload_slices[rt.vertex_pin.0 as usize];
    #[allow(clippy::cast_precision_loss)]
    let trap_pc = TrapDrawPushConsts {
        mask_extent: [mask_extent.width as f32, mask_extent.height as f32],
        bbox_origin_pixel: [rt.bbox_x as f32, rt.bbox_y as f32],
        bbox_size_pixel: [rt.bbox_w as f32, rt.bbox_h as f32],
        _pad: [0.0; 2],
    };
    #[allow(clippy::cast_precision_loss)]
    let trap_viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: mask_extent.width as f32,
        height: mask_extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    unsafe {
        device.cmd_begin_rendering(cb, &rendering_info);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, prim_pipeline);
        device.cmd_bind_vertex_buffers(cb, 0, &[vertex.buffer], &[vertex.offset]);
        device.cmd_push_constants(
            cb,
            prim_layout,
            vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
            0,
            trap_pc.as_bytes(),
        );
        device.cmd_set_viewport(cb, 0, &trap_viewport);
        device.cmd_set_scissor(cb, 0, &[bbox_render_area]);
        device.cmd_draw(cb, 4, rt.instance_count, 0, 0);
        device.cmd_end_rendering(cb);
    }

    // Barrier mask: COLOR_ATTACHMENT → SHADER_READ_ONLY for the composite read.
    let to_read = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
        .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .image(mask_image)
        .subresource_range(color_range)];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
    unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

    // ── (e) Composite phase — mirror engine.rs:7665-7735 ──

    // dst_readback snapshot for Disjoint/Conjoint.
    if rt.std_op.needs_dst_readback() {
        let dst_current = store
            .get(rt.dst_id)
            .expect("dst_id checked at append")
            .storage
            .current_layout;
        let rb = inner
            .dst_readback
            .as_mut()
            .expect("dst_readback: ensured at append");
        rb.record_copy_from(cb, rt.dst_image, dst_current, rt.dst_format, rt.dst_extent);
    }

    // needs_full_dst byte-pattern test from rt.op_byte (N5).
    let needs_full_dst = matches!(
        rt.op_byte,
        0 | 1 | 5 | 6 | 7 | 10 | 13 | 16..=27 | 32..=43
    );
    let (render_dst_x, render_dst_y, render_w, render_h, mask_off_x, mask_off_y) = if needs_full_dst
    {
        (
            0,
            0,
            rt.dst_extent.width,
            rt.dst_extent.height,
            -rt.bbox_x,
            -rt.bbox_y,
        )
    } else {
        (rt.bbox_x, rt.bbox_y, rt.bbox_w, rt.bbox_h, 0, 0)
    };

    // Compose src_xform: Gradient composes intrinsic; others pass user_src_xform.
    let combined_src_xform = match &rt.src_kind {
        crate::kms::render::frame_builder::RecordedTrapSrcKind::Gradient {
            intrinsic_axis_projection,
            ..
        } => crate::kms::backend::compose_affines(*intrinsic_axis_projection, rt.user_src_xform),
        _ => rt.user_src_xform,
    };
    // Effective src_repeat: PAD for synthetic 1×1, else recorded constant.
    // Cast back to i32 for CompositeAttrs (the shader constants are 0..=3).
    #[allow(clippy::cast_possible_wrap)]
    let effective_src_repeat: i32 = if rt.src_is_synthetic_1x1 {
        crate::kms::vk::render_pipeline::REPEAT_PAD
    } else {
        rt.src_repeat as i32
    };

    let attrs = vk_render::CompositeAttrs {
        src_extent: rt.src_extent,
        mask_extent,
        // #133 step 3 (P4): the recorded source's content origin. The
        // coverage mask is authored in dst space, so only the source
        // side carries an offset here.
        src_offset: match &rt.src_kind {
            crate::kms::render::frame_builder::RecordedTrapSrcKind::Drawable {
                sample_offset,
                ..
            } => [sample_offset.0, sample_offset.1],
            _ => [0, 0],
        },
        mask_offset: [0, 0],
        src_repeat: effective_src_repeat,
        mask_repeat: crate::kms::vk::render_pipeline::REPEAT_NONE,
        src_force_opaque: rt.src_force_opaque,
        mask_force_opaque: false,
        src_xform: combined_src_xform,
        mask_xform: vk_render::AffineXform::IDENTITY,
    };

    // Source sampling origin. Mirrors Xorg `miTrapezoids`: the src is
    // sampled at `xSrc + dst_px`. `rt.src_origin_{x,y}` already carries
    // the redirect/x_off-shifted `xSrc`/`ySrc`. For the `needs_full_dst`
    // branch the composite renders over the whole dst (the mask carries
    // the bbox offset via `mask_off`), so the src aligns directly at the
    // recorded origin; otherwise the composite renders at the bbox
    // origin and the src must add it back. Confined to `Drawable`
    // sources — `Solid` is a constant colour (origin irrelevant) and
    // `Gradient` is positioned by its intrinsic transform — so the only
    // behaviour change vs the prior hardcoded `0` is the picture-source
    // case (e.g. GTK CSD shadow blur-mask ramps sampled at `ySrc != 0`).
    let (src_org_x, src_org_y) = match &rt.src_kind {
        crate::kms::render::frame_builder::RecordedTrapSrcKind::Drawable { .. } => (
            trap_composite_src_origin_axis(rt.src_origin_x, rt.bbox_x, needs_full_dst),
            trap_composite_src_origin_axis(rt.src_origin_y, rt.bbox_y, needs_full_dst),
        ),
        _ => (0, 0),
    };

    let rects = [vk_render::CompositeRect {
        src_x: src_org_x,
        src_y: src_org_y,
        mask_x: mask_off_x,
        mask_y: mask_off_y,
        dst_x: render_dst_x,
        dst_y: render_dst_y,
        width: render_w,
        height: render_h,
    }];

    // Phase B.3 fix: under deferred recording, the GPU dst layout may
    // diverge from `storage.current_layout` — prior ops in the SAME
    // frame transitioned the dst on the GPU but storage isn't committed
    // until `commit_close_success` reads back from the frame overlay on
    // submit success. Driving the `to_color` barrier from
    // `storage.current_layout` here mis-declares old_layout to the
    // implementation, producing driver-undefined dst contents — the
    // observed symptom was partial α loss on depth-32 backings when
    // marco's frame trapezoids followed an inner-window `render_composite`
    // in the same frame.
    //
    // Match the B.2 render_composite emit path: use `RecordedCompositeTarget`
    // (constant `COLOR_ATTACHMENT_OPTIMAL`, non-mutating storage) and
    // `record_render_composite_open_with_old_layout` with the recorded
    // `dst_old_layout` from the overlay-resolved append snapshot. Storage
    // is NOT mutated here; `commit_close_success` writes the frame
    // overlay's post-op layout back to `storage.current_layout` on
    // successful submit. The append-time
    // `push_op_and_set_layouts([(dst_id, SHADER_READ_ONLY_OPTIMAL)])`
    // call records that post-op layout in the overlay.
    let mut adapter = RecordedCompositeTarget {
        image: rt.dst_image,
        view: rt.dst_view,
        extent: rt.dst_extent,
    };

    vk_render::record_render_composite_open_with_old_layout(
        &inner.vk,
        cb,
        &adapter,
        pipeline,
        rt.dst_old_layout,
    )?;
    vk_render::record_render_composite_draws(
        &inner.vk,
        cb,
        pipeline_layout,
        descriptor_set,
        rt.dst_extent,
        &attrs,
        &rects,
        &rt.clip_scissors,
    );
    vk_render::record_render_composite_close(&inner.vk, cb, &mut adapter);

    // ── (f) Post-emit CPU writeback (N5 LOAD-BEARING, codex round-10) ──
    // The composite-close barrier (inside record_render_composite) left the
    // mask_scratch image in SHADER_READ_ONLY_OPTIMAL on the GPU.
    // Advance the CPU-tracked layout NOW so the NEXT trap op's pre-barrier
    // emits from the correct old_layout instead of the stale pre-raster value.
    inner
        .mask_scratch
        .as_mut()
        .expect("mask_scratch: ensured at append")
        .set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);

    Ok(())
}

impl CompositeTarget for RecordedCompositeTarget {
    fn vk_image(&self) -> vk::Image {
        self.image
    }
    fn vk_image_view(&self) -> vk::ImageView {
        self.view
    }
    fn extent(&self) -> vk::Extent2D {
        self.extent
    }
    fn current_layout(&self) -> vk::ImageLayout {
        // See struct doc — `_with_old_layout` doesn't read this; the
        // close path doesn't read it either. Return the layout the
        // image IS in between open and close (a constant) as
        // defence-in-depth against a future refactor that adds a read.
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
    }
    fn set_current_layout(&mut self, _layout: vk::ImageLayout) {
        // Intentional no-op — see struct doc. Codex R5 audit point.
    }
}

fn color_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

/// Single-image-layout barrier helper for scratch images that
/// `Drawable::record_layout_transition` can't touch (the scratch
/// isn't a tracked drawable).
#[allow(clippy::too_many_arguments)]
fn barrier_to_layout(
    device: &ash::Device,
    cb: vk::CommandBuffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
    src_stage: vk::PipelineStageFlags2,
    src_access: vk::AccessFlags2,
    dst_stage: vk::PipelineStageFlags2,
    dst_access: vk::AccessFlags2,
) {
    let b = [vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src_stage)
        .src_access_mask(src_access)
        .dst_stage_mask(dst_stage)
        .dst_access_mask(dst_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1),
        )];
    let dep = vk::DependencyInfo::default().image_memory_barriers(&b);
    unsafe { device.cmd_pipeline_barrier2(cb, &dep) };
}

/// Stage 5 Task 3: build clip-scissor list for render_composite
/// (mirrors the inline arithmetic in `render_composite`). `None`
/// → single full-extent scissor; `Some(cr)` → clamped per-rect
/// list (empty rects skipped). Returns empty Vec if no rect is
/// visible.
pub(super) fn build_render_clip_scissors(
    clip_rects: Option<&[Rectangle16]>,
    dst_extent: vk::Extent2D,
) -> Vec<vk::Rect2D> {
    build_render_clip_scissors_to(
        clip_rects,
        vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: dst_extent,
        },
    )
}

/// #133 step 3 (P4) — [`build_render_clip_scissors`] against an
/// arbitrary bounds rect. `clip_rects == None` ("no picture clip")
/// yields the bounds itself, so a RENDER destination is scissored to the
/// content rect even when the client supplied no clip of its own.
pub(super) fn build_render_clip_scissors_to(
    clip_rects: Option<&[Rectangle16]>,
    bounds: vk::Rect2D,
) -> Vec<vk::Rect2D> {
    let min_x = bounds.offset.x;
    let min_y = bounds.offset.y;
    let max_x = bounds.offset.x.saturating_add_unsigned(bounds.extent.width);
    let max_y = bounds
        .offset
        .y
        .saturating_add_unsigned(bounds.extent.height);
    match clip_rects {
        None => vec![bounds],
        Some(cr) => {
            let mut out = Vec::with_capacity(cr.len());
            for r in cr {
                if r.width == 0 || r.height == 0 {
                    continue;
                }
                let x0 = i32::from(r.x).max(min_x);
                let y0 = i32::from(r.y).max(min_y);
                let x1 = (i32::from(r.x) + i32::from(r.width)).min(max_x);
                let y1 = (i32::from(r.y) + i32::from(r.height)).min(max_y);
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                out.push(vk::Rect2D {
                    offset: vk::Offset2D { x: x0, y: y0 },
                    extent: vk::Extent2D {
                        #[allow(clippy::cast_sign_loss)]
                        width: (x1 - x0) as u32,
                        #[allow(clippy::cast_sign_loss)]
                        height: (y1 - y0) as u32,
                    },
                });
            }
            out
        }
    }
}
