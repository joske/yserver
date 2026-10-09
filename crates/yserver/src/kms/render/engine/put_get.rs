use super::*;

impl RenderEngine {
    /// Drain the accumulated `get_image` phase totals, zeroing them.
    /// Returns `None` when nothing accrued since the last drain, so the
    /// caller can skip a no-op telemetry record.
    pub(crate) fn drain_get_image_phases(&mut self) -> Option<GetImagePhases> {
        let inner = self.inner.as_mut()?;
        let totals = std::mem::take(&mut inner.get_image_phase_totals);
        (totals != GetImagePhases::default()).then_some(totals)
    }

    // ── Op: put_image ───────────────────────────────────────────

    /// Upload `src_bytes` (interpreted per `src_depth`) into
    /// `target` at `dst_pos`. Stage 2c supports depths 1, 8, 24,
    /// 32 with the byte layouts the X11 dispatcher emits (see
    /// the inline conversion table). Per-op staging buffer; no
    /// arena coalescing yet.
    ///
    /// # Errors
    ///
    /// - `UnsupportedDepth` if `src_depth` isn't 1/8/24/32.
    /// - `TruncatedSource` if `src_bytes` is shorter than the
    ///   row stride × height the depth implies.
    /// - `Vk(...)` for any Vk failure (CB / buffer / submit).
    pub(crate) fn put_image(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        dst: Dst,
        dst_pos: vk::Offset2D,
        src_extent: vk::Extent2D,
        src_bytes: &[u8],
        src_depth: u8,
    ) -> Result<(), RenderError> {
        let target = dst.id();
        // Phase B.3 (N9): empty-input fast-path FIRST — before renderer_failed
        // and flush_render_batch.
        if src_extent.width == 0 || src_extent.height == 0 {
            return Ok(());
        }
        // Phase B.3 (N9): renderer_failed check before any open-frame mutation.
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        // Phase B.3 (N9): flush pending_render_batch at entry. May close an
        // open frame (chronological X11 ordering with pre-existing batches).
        // No flush_cow_batch — that helper is deleted in Task 4.
        self.flush_render_batch(store, platform, RenderFlushReason::PutImage)?;

        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        let Some(drawable) = store.get(target) else {
            return Err(RenderError::UnknownDrawable(target));
        };

        // Stage 2c-supported depths only. Anything else is logged
        // upstream and routes to the gap path; we surface the
        // type-level reject so the backend wrapper can dedup-log.
        let dst_bpp: u32 = match src_depth {
            1 | 4 | 8 => 1,
            24 | 32 => 4,
            _ => return Err(RenderError::UnsupportedDepth(src_depth)),
        };
        let dst_format = drawable.storage.format;
        // The store allocates storage by depth; format mismatch
        // here means the caller targeted a depth-mismatched
        // drawable. Treat as unsupported.
        let expected_format = if dst_bpp == 1 {
            vk::Format::R8_UNORM
        } else {
            vk::Format::B8G8R8A8_UNORM
        };
        if dst_format != expected_format {
            return Err(RenderError::UnsupportedDepth(src_depth));
        }

        let dst_extent = drawable.storage.extent;
        let dst_image = drawable.storage.image;
        let dst_pre_layout = inner.current_layout_for_drawable(store, target);
        let prior_dst_ticket = drawable.last_render_ticket.clone();

        // Clamp the put rect to the destination BOUNDS (#133 step 3 (P4):
        // the content rect for a bordered window, the whole storage
        // otherwise). The returned source offset crops the wire image, so
        // a PutImage at a negative destination coordinate loses its
        // leading rows/columns instead of writing them into the ring.
        // Per Stage 2
        // plan, GC clipping is the backend wrapper's concern;
        // the engine only sees the dst-extent guard.
        let clipped = clamp_put_rect_to(dst_pos, src_extent, dst.bounds_in(dst_extent));
        let Some((dst_rect, src_origin_in_input)) = clipped else {
            return Ok(());
        };
        let copy_w = dst_rect.extent.width;
        let copy_h = dst_rect.extent.height;
        let staging_size = u64::from(copy_w) * u64::from(copy_h) * u64::from(dst_bpp);
        if staging_size == 0 {
            return Ok(());
        }

        // Phase B.3 (N8-style ordering): allocate the staging buffer BEFORE
        // any open-frame mutation so an allocation failure leaves the frame
        // untouched (no rollback needed).
        // #nvidia perf: reuse a pooled upload staging buffer instead of a fresh
        // vkCreateBuffer+vkAllocateMemory per put_image (costly on NVIDIA).
        // Returned to the pool at retire (poll_retired). Clone vk to a local
        // first so the &mut borrow of `inner.staging_pool` doesn't alias `inner.vk`.
        let staging_vk = inner.vk.clone();
        let staging = Arc::new(
            inner
                .staging_pool
                .acquire(&staging_vk, staging_size.max(1))?,
        );
        // Convert src_bytes → staging according to (depth, dst_format).
        let (sx, sy) = src_origin_in_input;
        unpack_to_staging(
            src_bytes,
            src_extent,
            sx,
            sy,
            copy_w,
            copy_h,
            src_depth,
            staging.mapped.as_ptr(),
        )?;

        // Open the frame if not already open. Phase B.2 Mechanism 2: bump
        // acquire_generation at open + capture on OpenFrame. (Same pattern as
        // composite_glyphs_via_frame_builder.)
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

        // Phase B.3 (N2): pin the staging Arc into the frame pin-set BEFORE
        // any `store` mutation (first_touch + damage happen after pinning so
        // that a pin-failure doesn't leave store state inconsistent).
        let staging_pin_idx = {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(target, prior_dst_ticket);
            open.layouts.first_touch_drawable(target, dst_pre_layout);
            open.pins.pin_staging(Arc::clone(&staging))
        };
        store.touch_render_fence(target, frame_ticket.clone());
        store.damage(target, dst_rect);

        // Phase B.3 (N1): push the op and set the terminal layout
        // SHADER_READ_ONLY_OPTIMAL for the dst.
        let payload = Box::new(crate::kms::render::frame_builder::RecordedPutImage {
            dst_id: target,
            dst_rect,
            dst_image,
            dst_extent,
            dst_old_layout: dst_pre_layout,
            staging_pin_idx,
        });
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.push_op_and_set_layouts(
                crate::kms::render::frame_builder::RecordedOp::PutImage(payload),
                &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
            );
        }
        store.mark_contents_modified(target);
        Ok(())
    }

    // ── Op: get_image (synchronous) ─────────────────────────────

    /// Read `rect` from `src`'s storage. **Synchronous** — waits
    /// on the readback `FenceTicket` before returning. The only
    /// sync path on the v2 paint surface; protocol design makes
    /// `GetImage` an RPC, so a host wait is unavoidable.
    ///
    /// Returns bytes in **wire format** (see `pack_from_storage`):
    /// for depth-32/24, `rect_w * rect_h * 4` BGRA-order bytes
    /// (alpha undefined for depth-24). For depth-8, byte rows
    /// padded to 32 bits. For depth-1, bitmap rows padded to 32
    /// bits, LSBFirst bit order; storage is `R8` and each non-zero
    /// byte sets one bit. All layouts keep the total a multiple of
    /// 4, which `wrap_get_image_reply` relies on for the reply
    /// length field.
    ///
    /// # Errors
    ///
    /// - `UnsupportedDepth` for depths other than 1/8/24/32.
    /// - `Vk` for CB / buffer / submit / wait failures.
    pub(crate) fn get_image(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        src_handle: Src,
        rect: vk::Rect2D,
        out_depth: u8,
    ) -> Result<Vec<u8>, RenderError> {
        let src = src_handle.id();
        // get_image is a synchronous CPU readback — must see all
        // prior submits including any pending COW batch.
        //
        // Per-phase timing (cinnamon-on-NVIDIA chop diagnosis): the
        // non-TFP compositor fallback issues XShmGetImage every frame and
        // each call blocks the single-threaded loop here. Stamp each phase
        // so a slow call (>=15ms) logs WHERE the time went — distinguishes
        // "blocked draining the in-flight compose" (close_frame/flush) from
        // "blocked on the readback fence" (wait). Cheap: a few Instant reads
        // per GetImage; the log only fires on the slow tail.
        let t_start = std::time::Instant::now();
        self.flush_render_batch(store, platform, RenderFlushReason::Readback)?;
        let t_after_batch = std::time::Instant::now();
        // Phase B.1 close trigger 2: close any open frame before the
        // readback's ticket.wait(). The frame's CB must submit before the
        // readback CB records; without this, the readback would race the
        // deferred frame.
        self.close_open_frame(
            store,
            platform,
            crate::kms::render::frame_builder::CloseReason::SyncWait,
        )?;
        let t_after_close = std::time::Instant::now();
        // Phase A: drain any buffered paint group BEFORE allocating the
        // readback CB. This ensures prior paint ops are queued/submitted
        // so the readback observes them. Distinct from the second
        // flush below — which signals the readback's own fence so
        // ticket.wait() observes a queued signal-op. Both are needed:
        // this one drains prior buffered paint; the second flushes the
        // readback CB itself.
        self.flush_submit_group(
            store,
            platform,
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        )
        .map_err(RenderError::Vk)?;
        let t_after_flush1 = std::time::Instant::now();
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };
        if platform.renderer_failed {
            return Err(RenderError::RendererFailed);
        }
        let Some(drawable) = store.get_mut(src) else {
            return Err(RenderError::UnknownDrawable(src));
        };
        let storage_bpp: u32 = match out_depth {
            1 | 4 | 8 => 1,
            24 | 32 => 4,
            _ => return Err(RenderError::UnsupportedDepth(out_depth)),
        };
        let extent = drawable.storage.extent;
        // Clamp the read rect to the SOURCE HANDLE's bounds (#133 step 3
        // (P4)): GetImage on a bordered window must not return ring
        // pixels as window content.
        let clipped = clamp_rect_to(rect, src_handle.bounds_in(extent));
        let copy_w = clipped.extent.width;
        let copy_h = clipped.extent.height;
        if copy_w == 0 || copy_h == 0 {
            return Ok(Vec::new());
        }
        let staging_size = u64::from(copy_w) * u64::from(copy_h) * u64::from(storage_bpp);
        // Readback staging: HOST_CACHED-preferred so the CPU pack below reads
        // at cached-RAM speed. Plain HOST_COHERENT is write-combined on
        // discrete GPUs and made this pack 50–90ms for a full-screen read
        // (project_cinnamon_nvidia_chop_shm_getimage).
        let staging = Arc::new(StagingBuffer::new_for_readback(
            inner.vk.clone(),
            staging_size.max(1),
        )?);

        let (cb, ticket) = begin_op_cb(inner, platform)?;
        let device = &inner.vk.device;

        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );

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
                x: clipped.offset.x,
                y: clipped.offset.y,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: copy_w,
                height: copy_h,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image_to_buffer(
                cb,
                drawable.storage.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                staging.buffer,
                &region,
            );
        }

        drawable.record_layout_transition(
            &inner.vk,
            cb,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );

        end_and_submit_op(inner, platform, cb, &ticket)?;
        store.touch_render_fence(src, ticket.clone());
        // `inner` borrow released before flush so self.flush_submit_group
        // can take &mut self.
        let _ = inner;
        let t_after_record = std::time::Instant::now();

        // Phase A: end_and_submit_op now only appends to the SubmitGroup.
        // Drive the explicit flush so the fence has a queued signal-op
        // before we wait on it.
        self.flush_submit_group(
            store,
            platform,
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        )
        .map_err(RenderError::Vk)?;
        let t_after_flush2 = std::time::Instant::now();
        let Some(inner) = self.inner.as_mut() else {
            return Err(RenderError::NoVk);
        };

        // Sync wait — off the hot path by protocol design.
        ticket.wait(&inner.vk)?;
        // Make the GPU's writes visible to the CPU reads below. No-op for a
        // HOST_COHERENT staging buffer; required for the HOST_CACHED-only
        // readback type new_for_readback may have selected.
        staging.invalidate_for_read()?;
        let t_after_wait = std::time::Instant::now();

        // Pack storage bytes into wire format.
        let raw_size = (u64::from(copy_w) * u64::from(copy_h) * u64::from(storage_bpp)) as usize;
        // SAFETY: staging is mapped for `staging.size` bytes (≥ raw_size),
        // the fence above signalled so the GPU has completed all writes, and
        // invalidate_for_read made those writes visible to the CPU.
        let raw: &[u8] = unsafe { std::slice::from_raw_parts(staging.mapped.as_ptr(), raw_size) };
        let out = pack_from_storage(raw, copy_w, copy_h, out_depth)?;
        let t_after_pack = std::time::Instant::now();

        // Carry the phase split out on EVERY call (the slow-tail log below
        // only fires above GET_IMAGE_SLOW_MS, which hides the aggregate the
        // deferred-readback decision needs). `setup_record`/`flush2` are
        // folded into `drain` — they are submit work, same as the flushes,
        // and a deferred readback does not remove them either.
        let ns = |a: std::time::Instant, b: std::time::Instant| {
            u64::try_from(b.duration_since(a).as_nanos()).unwrap_or(u64::MAX)
        };
        let totals = &mut inner.get_image_phase_totals;
        totals.drain_ns = totals.drain_ns.saturating_add(
            ns(t_start, t_after_flush1).saturating_add(ns(t_after_flush1, t_after_flush2)),
        );
        totals.wait_ns = totals
            .wait_ns
            .saturating_add(ns(t_after_flush2, t_after_wait));
        totals.copyout_ns = totals
            .copyout_ns
            .saturating_add(ns(t_after_wait, t_after_pack));

        // Per-phase breakdown for the cinnamon-on-NVIDIA chop diagnosis
        // (project_cinnamon_nvidia_chop_shm_getimage). Gated on the same
        // YSERVER_LOOP_TELEMETRY toggle as the rest of v2 telemetry, and
        // emitted in the same grep/awk-parsable `key=value` line format so it
        // sits alongside the `render_telemetry:` lines. Only the slow tail
        // (>= GET_IMAGE_SLOW_MS) logs, so the common fast read stays silent and
        // the 50-300ms outliers stand out. `wait_ms` dominating ⇒ blocked on
        // the readback fence (behind the in-flight compose); `close_frame_ms`/
        // `flush1_ms` dominating ⇒ blocked draining the compositor frame.
        let total_ms = t_after_pack.duration_since(t_start).as_secs_f64() * 1000.0;
        if total_ms >= GET_IMAGE_SLOW_MS && get_image_phase_telemetry_enabled() {
            let ms = |a: std::time::Instant, b: std::time::Instant| {
                b.duration_since(a).as_secs_f64() * 1000.0
            };
            log::info!(
                "get_image_phase: total_ms={:.1} flush_batch_ms={:.1} close_frame_ms={:.1} \
                 flush1_ms={:.1} setup_record_ms={:.1} flush2_ms={:.1} wait_ms={:.1} pack_ms={:.1} \
                 w={} h={} depth={} src={}",
                total_ms,
                ms(t_start, t_after_batch),
                ms(t_after_batch, t_after_close),
                ms(t_after_close, t_after_flush1),
                ms(t_after_flush1, t_after_record),
                ms(t_after_record, t_after_flush2),
                ms(t_after_flush2, t_after_wait),
                ms(t_after_wait, t_after_pack),
                copy_w,
                copy_h,
                out_depth,
                src.as_u64(),
            );
        }

        // `get_image` is the ONLY exception to the
        // `pending_group_ops`-on-paint-op rule. We push direct to
        // `submitted` because the fence is already signaled (we waited
        // on it above) and `staging.mapped` was read BEFORE we could
        // have moved staging into `pending_group_ops` (lifetime
        // requirement). `poll_retired` retires this op on the next tick.
        inner.acquire_generation += 1;
        let generation = inner.acquire_generation;
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: Some(staging),
            scratch: Vec::new(),
            sampled_scratch: Vec::new(),
            atlas_ticket: None,
            generation,
            retired_resources: Vec::new(),
        });

        Ok(out)
    }
}

/// `get_image` calls slower than this (wall-clock ms) emit a
/// `get_image_phase:` telemetry line. Tuned to fire only on the stall tail
/// (the chop is 50-300ms; normal reads are sub-ms) so the line stays quiet
/// during healthy operation. See `RenderEngine::get_image`.
const GET_IMAGE_SLOW_MS: f64 = 15.0;

/// Whether to emit `get_image_phase:` lines — gated on the same
/// `YSERVER_LOOP_TELEMETRY` env toggle as [`crate::kms::render::telemetry::Telemetry`]
/// (read once, cached). Keeps the per-phase diagnostic silent unless a
/// deliberate telemetry session is requested.
fn get_image_phase_telemetry_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var_os("YSERVER_LOOP_TELEMETRY")
                .as_deref()
                .and_then(|s| s.to_str()),
            Some("1" | "true" | "yes" | "on")
        )
    })
}

/// Compute the destination rect (in storage coords) and the
/// (sx, sy) origin in the input image where copying should start.
/// Returns `None` if no pixels are visible.
pub(super) fn clamp_put_rect(
    dst_pos: vk::Offset2D,
    src_extent: vk::Extent2D,
    dst_extent: vk::Extent2D,
) -> Option<(vk::Rect2D, (u32, u32))> {
    clamp_put_rect_to(
        dst_pos,
        src_extent,
        vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: dst_extent,
        },
    )
}

/// #133 step 3 (P4) — [`clamp_put_rect`] against an arbitrary bounds
/// rect. The returned source offset `(sx, sy)` is what crops the wire
/// image: a `PutImage` whose destination rect starts left of / above the
/// content origin has its leading columns and rows skipped rather than
/// written into the border ring.
pub(super) fn clamp_put_rect_to(
    dst_pos: vk::Offset2D,
    src_extent: vk::Extent2D,
    bounds: vk::Rect2D,
) -> Option<(vk::Rect2D, (u32, u32))> {
    let min_x = bounds.offset.x;
    let min_y = bounds.offset.y;
    let max_x = bounds.offset.x.saturating_add_unsigned(bounds.extent.width);
    let max_y = bounds
        .offset
        .y
        .saturating_add_unsigned(bounds.extent.height);
    let x0 = dst_pos.x.max(min_x);
    let y0 = dst_pos.y.max(min_y);
    let sx = (x0 - dst_pos.x).max(0);
    let sy = (y0 - dst_pos.y).max(0);
    let x1 = dst_pos
        .x
        .saturating_add_unsigned(src_extent.width)
        .min(max_x);
    let y1 = dst_pos
        .y
        .saturating_add_unsigned(src_extent.height)
        .min(max_y);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some((
        vk::Rect2D {
            offset: vk::Offset2D { x: x0, y: y0 },
            extent: vk::Extent2D {
                width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
                height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
            },
        },
        (
            u32::try_from(sx).unwrap_or(0),
            u32::try_from(sy).unwrap_or(0),
        ),
    ))
}
