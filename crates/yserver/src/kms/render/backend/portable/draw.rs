use super::*;

impl KmsBackend {
    /// Compute the surviving destination scissor rects for a CopyArea, in
    /// drawable-LOCAL space (before `dst_target.offset()` is added). This is the
    /// EXACT non-mask clip machinery the run-based path uses, in order: GC
    /// clip-rect intersect (`ClipState::Rectangles`; `Pixmap`/`None` keep the
    /// whole copy rect), then ClipByChildren child-window subtraction (window
    /// dsts only), then higher-sibling occluder subtraction (shared redirect
    /// backing). Extracted from `copy_area`'s `post_gc_clip` →
    /// `child_clipped_rects` → `sub_rects` block so both the run-based path and
    /// the GPU masked path (Task 14) share identical non-mask clipping. The
    /// masked path further gates by the GPU clip-mask snapshot and shifts these
    /// rects into image space by `dst_target.offset()`. An empty result means
    /// "fully clipped away" (empty GC clip, or fully occluded): a no-op.
    pub(in crate::kms::render::backend) fn compute_copy_area_scissors(
        &self,
        dst_host_xid: u32,
        dst_target: &PaintTarget,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> Vec<ash::vk::Rect2D> {
        let dst_rect_local = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(dst_x),
                y: i32::from(dst_y),
            },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        // #133 step 3 (P4): confine the copy to the destination's content
        // BEFORE the clip machinery, so "fully clipped away" (the
        // `is_empty()` early-out both callers rely on) also covers a copy
        // that lands entirely in the border ring. The engine clamps again
        // on dispatch; this is the local-space mirror. Identity when the
        // destination has no border clip.
        let Some(dst_rect_local) = dst_target.clip_local_vk_rect(dst_rect_local) else {
            return Vec::new();
        };
        let post_gc_clip: Vec<ash::vk::Rect2D> =
            if let yserver_core::backend::ClipState::Rectangles { origin, rects } =
                &self.core.current_clip
            {
                let clip_rects: Vec<ash::vk::Rect2D> = rects
                    .rectangles
                    .chunks_exact(8)
                    .filter_map(|chunk| {
                        let cx = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]))
                            + i32::from(origin.0);
                        let cy = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]))
                            + i32::from(origin.1);
                        let cw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
                        let ch = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
                        if cw <= 0 || ch <= 0 {
                            return None;
                        }
                        Some(ash::vk::Rect2D {
                            offset: ash::vk::Offset2D { x: cx, y: cy },
                            extent: ash::vk::Extent2D {
                                width: u32::try_from(cw).unwrap_or(0),
                                height: u32::try_from(ch).unwrap_or(0),
                            },
                        })
                    })
                    .collect();
                intersect_rect_with_clip(dst_rect_local, &clip_rects)
            } else {
                vec![dst_rect_local]
            };
        if post_gc_clip.is_empty() {
            return Vec::new();
        }
        // Step 2: ClipByChildren — subtract every mapped child window
        // rect from each post-GC-clip rect. IncludeInferiors (mode=1)
        // keeps each post-GC-clip rect as-is. Pixmap destinations
        // (not in `windows`) also bypass child subtraction.
        let child_clipped_rects: Vec<ash::vk::Rect2D> =
            if matches!(
                self.core.current_subwindow_mode,
                yserver_core::backend::SubwindowMode::ClipByChildren,
            ) && self.windows.contains_key(&dst_host_xid)
            {
                let child_rects: Vec<ash::vk::Rect2D> = self
                    .windows
                    .iter()
                    .filter_map(|(child_host_xid, geom)| {
                        if !(geom.parent == Some(dst_host_xid) && geom.mapped) {
                            return None;
                        }
                        // Manually-redirected children don't claim the
                        // parent's pixmap real estate (see run-path note).
                        let is_manually_redirected = self
                            .store
                            .lookup(*child_host_xid)
                            .and_then(|id| self.store.get(id))
                            .is_some_and(|d| !d.scene_participating);
                        if is_manually_redirected {
                            return None;
                        }
                        // #133 step 3 round 5 — child rects live in the
                        // parent's CONTENT space: `(x + bw, y + bw)`.
                        // Same rule as the `IncludeInferiors` fan-out and
                        // `clip_fill_rects_by_subwindow_mode`; identity
                        // at `bw == 0`.
                        let child_bw = i32::from(geom.border_width);
                        let content_box = ash::vk::Rect2D {
                            offset: ash::vk::Offset2D {
                                x: i32::from(geom.x) + child_bw,
                                y: i32::from(geom.y) + child_bw,
                            },
                            extent: ash::vk::Extent2D {
                                width: u32::from(geom.width.max(1)),
                                height: u32::from(geom.height.max(1)),
                            },
                        };
                        Some(self.child_clip_region(*child_host_xid, geom, content_box))
                    })
                    .flatten()
                    .collect();
                if child_rects.is_empty() {
                    post_gc_clip
                } else {
                    post_gc_clip
                        .into_iter()
                        .flat_map(|r| compute_copy_area_dst_rects(r, &child_rects))
                        .collect()
                }
            } else {
                post_gc_clip
            };
        // Higher siblings at every level, and the bounding shapes, of a
        // window sharing its ancestors' backing.
        match self.shared_backing_draw_clip(dst_host_xid, dst_target) {
            Some(keep) => child_clipped_rects
                .into_iter()
                .flat_map(|r| intersect_rect_with_clip(r, &keep))
                .collect(),
            None => child_clipped_rects,
        }
    }

    pub(in crate::kms::render::backend) fn fill_solid_rects(
        &mut self,
        target: PaintTarget,
        fg: u32,
        rects: &[Rectangle16],
    ) {
        self.fill_solid_rects_with(
            target,
            fg,
            rects,
            self.core.current_function,
            self.core.current_plane_mask,
        );
    }

    /// [`Self::fill_solid_rects`] with the raster op and plane mask
    /// given rather than taken from the last client GC, so a
    /// server-internal paint can state its own.
    pub(in crate::kms::render::backend) fn fill_solid_rects_with(
        &mut self,
        target: PaintTarget,
        fg: u32,
        rects: &[Rectangle16],
        function: yserver_core::backend::GcFunction,
        plane_mask: u32,
    ) {
        use yserver_core::backend::GcFunction;
        if rects.is_empty() {
            return;
        }
        if matches!(function, GcFunction::NoOp) {
            return;
        }
        let (dx, dy) = target.offset();
        let id = target.backing_id();
        let logical_depth = target.x11_depth();
        let Some((_storage_depth, format, extent)) = self
            .store
            .get(id)
            .map(|d| (d.depth, d.storage.format, d.storage.extent))
        else {
            return;
        };
        let full_mask = depth_plane_mask(logical_depth);
        let plane_mask = plane_mask & full_mask;
        if plane_mask == 0 {
            return;
        }
        let shifted = Self::shift_rectangles_for_paint(rects, target.offset());
        // Depth-1 GXcopy fast path: route to the GPU R8 fill instead of the
        // get_image + per-pixel RMW + put_image CPU fallback. The R8 fill is
        // correct for GXcopy because:
        //   - decode_x11_pixel_for_storage(fg & 1, 1, R8_UNORM) → [fg&1, 0, 0, 0]
        //   - pack_from_storage packs any nonzero R8 byte as the set bit (LSB)
        // depth-1 non-Copy (boolean-logic hazard in R8 byte-wise logic ops) and
        // depth-4 (no equivalence proof) stay on the CPU fallback path.
        if logical_depth == 1 && plane_mask == full_mask && matches!(function, GcFunction::Copy) {
            let opaque_alpha = logical_depth != 32; // true for depth-1
            match self.engine.logic_fill(
                &mut self.store,
                &mut self.platform,
                target.dst(),
                GcFunction::Copy,
                opaque_alpha,
                fg & full_mask,
                &shifted,
            ) {
                Ok(()) => {
                    self.telemetry.record_paint_submit();
                    self.trace_simple(
                        SubmitKind::FillBatch,
                        id,
                        u32::try_from(shifted.len()).unwrap_or(u32::MAX),
                    );
                }
                Err(e) => {
                    log::warn!("render fill_solid_rects depth1 gpu copy: {e:?}");
                }
            }
            return;
        }
        if logical_depth < 8 || plane_mask != full_mask {
            // Round-2/3 disambiguation: depth<8 short-circuits the `||`, so it
            // is the reason whenever it holds; otherwise the partial plane
            // mask is. The GXcopy split (depth<8 only) decides whether the
            // depth-1 GPU fill (FIX B) is B1-only or also needs B2.
            self.telemetry
                .record_cpufill_fallback(logical_depth < 8, matches!(function, GcFunction::Copy));
            self.fill_solid_rects_cpu_fallback(
                target.dst(),
                extent,
                logical_depth,
                function,
                plane_mask,
                fg & full_mask,
                &shifted,
            );
            return;
        }
        if !matches!(function, GcFunction::Copy) {
            // Compute `opaque_alpha` per the L1 server-α invariant:
            // depth-32 ARGB destinations take the LogicOp on all four
            // channels; depth-24/8/1 are server-owned-α so the
            // pipeline's write mask drops alpha to keep the dst byte
            // intact. Depth lookup via the drawable record.
            let opaque_alpha = logical_depth != 32;
            match self.engine.logic_fill(
                &mut self.store,
                &mut self.platform,
                target.dst(),
                function,
                opaque_alpha,
                fg & full_mask,
                &shifted,
            ) {
                Ok(()) => {
                    // One submit per call regardless of rect count
                    // (logic_fill records every rect into the same CB).
                    self.telemetry.record_paint_submit();
                    let op_byte = function.protocol_value();
                    let target_kind = self.submit_target_kind(id);
                    self.telemetry.record_submit_event(SubmitEvent {
                        frame_id: 0,
                        kind: SubmitKind::LogicFill,
                        target_kind,
                        target_id: id.as_u64(),
                        batch_size: u32::try_from(shifted.len()).unwrap_or(u32::MAX),
                        op: SubmitOp::from_gx_byte(op_byte),
                        src_class: SrcClass::None,
                        mask_class: SrcClass::None,
                        pipeline_id: None,
                        flags: SubmitFlags::NONE,
                    });
                }
                Err(e) => {
                    log::warn!(
                        "render fill_solid_rects: engine.logic_fill failed ({function:?}): {e:?}"
                    );
                }
            }
            return;
        }
        // L1 server-α invariant: depth-24 dst stores alpha=0xFF
        // regardless of the X11 pixel's upper byte. Without this,
        // the scene compositor's alpha_passthrough=true draws read
        // back α=0 (X-padding) and the window blends transparent —
        // the layer underneath leaks through, panel renders white
        // not teal. Matches v1's `try_vk_solid_fill` (kms/backend.rs:3512).
        let color = decode_x11_pixel_for_storage(fg & full_mask, logical_depth, format);
        // Stage 3f.15: coalesce N stroke rects into one CB + one
        // submit via engine.fill_rect_batch. PolySegment / PolyLine
        // / PolyRectangle fan-outs now pay O(1) submits per protocol
        // request instead of O(N). Zero-sized rects are filtered
        // inside the engine.
        //
        // Stage 4a — apply paint-target offset (window-local →
        // backing-local) directly into the i32 vk::Offset2D.
        let vk_rects: Vec<ash::vk::Rect2D> = rects
            .iter()
            .filter(|r| r.width != 0 && r.height != 0)
            .map(|r| ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(r.x) + dx,
                    y: i32::from(r.y) + dy,
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                },
            })
            .collect();
        if vk_rects.is_empty() {
            return;
        }
        let n_rects = u32::try_from(vk_rects.len()).unwrap_or(u32::MAX);
        match self.engine.fill_rect_batch(
            &mut self.store,
            &mut self.platform,
            target.dst(),
            color,
            &vk_rects,
        ) {
            Ok(()) => {
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::FillBatch, id, n_rects);
            }
            Err(e) => {
                log::warn!("render fill_solid_rects: engine.fill_rect_batch failed: {e:?}");
            }
        }
    }

    /// CPU read-modify-write fill. The readback and the write-back span
    /// the WHOLE storage (the row geometry the pixel loop indexes with),
    /// so both use the PRIVILEGED backing route; the region actually
    /// modified is clipped to `dst`'s content bounds below (#133 step 3
    /// (P4)), which is what keeps the border ring out of the loop.
    fn fill_solid_rects_cpu_fallback(
        &mut self,
        dst: Dst,
        extent: ash::vk::Extent2D,
        depth: u8,
        function: yserver_core::backend::GcFunction,
        plane_mask: u32,
        fg: u32,
        rects: &[Rectangle16],
    ) {
        let id = dst.id();
        let bounds = dst.bounds_in(extent);
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::CpuFallbackFill);
        let mut bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            Src::server_internal(id),
            rect,
            depth,
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("render fill_solid_rects_cpu_fallback: get_image failed: {e:?}");
                return;
            }
        };
        let full_mask = depth_plane_mask(depth);
        // The clip is the content BOUNDS, not `[0, extent)` — identical at
        // `bw == 0`, where `bounds` IS the storage extent.
        let bx0 = bounds.offset.x;
        let by0 = bounds.offset.y;
        let bx1 = bounds.offset.x.saturating_add_unsigned(bounds.extent.width);
        let by1 = bounds
            .offset
            .y
            .saturating_add_unsigned(bounds.extent.height);
        for r in rects {
            let x0 = i32::from(r.x).max(bx0).max(0) as usize;
            let y0 = i32::from(r.y).max(by0).max(0) as usize;
            let x1 = (i32::from(r.x).saturating_add(i32::from(r.width))).min(bx1);
            let y1 = (i32::from(r.y).saturating_add(i32::from(r.height))).min(by1);
            if x1 <= x0 as i32 || y1 <= y0 as i32 {
                continue;
            }
            for y in y0..y1 as usize {
                for x in x0..x1 as usize {
                    let dst = read_z_pixmap_pixel(&bytes, depth, extent.width, x, y) & full_mask;
                    let out = apply_gc_function(function, fg, dst, plane_mask) & full_mask;
                    write_z_pixmap_pixel(&mut bytes, depth, extent.width, x, y, out);
                }
            }
        }
        // PRIVILEGED write-back: the bytes outside the (clipped) rects are
        // exactly what was just read, so this writes the ring back
        // unchanged rather than painting it.
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            Dst::server_internal(id),
            ash::vk::Offset2D::default(),
            extent,
            &bytes,
            depth,
        ) {
            log::warn!("render fill_solid_rects_cpu_fallback: put_image failed: {e:?}");
            return;
        }
        self.telemetry.record_paint_submit();
        self.trace_simple(
            if matches!(function, yserver_core::backend::GcFunction::Copy) {
                SubmitKind::FillBatch
            } else {
                SubmitKind::LogicFill
            },
            id,
            u32::try_from(rects.len()).unwrap_or(u32::MAX),
        );
    }

    /// CopyArea with a non-Copy GC function or partial plane-mask:
    /// CPU read-modify-write. Reads the source and destination
    /// sub-rects (clamped to both drawables), applies
    /// `apply_gc_function(src, dst)` per pixel, writes back. The
    /// conformance path only — real clients copy with GXcopy.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::kms::render::backend) fn copy_area_rop_cpu(
        &mut self,
        src_handle: Src,
        dst_handle: Dst,
        src_rect: ash::vk::Rect2D,
        dst_pos: ash::vk::Offset2D,
        function: yserver_core::backend::GcFunction,
        plane_mask: u32,
        depth: u8,
    ) {
        let src_id = src_handle.id();
        let dst_id = dst_handle.id();
        self.telemetry.record_copy_area_cpu_run();
        let Some(src_extent) = self.store.get(src_id).map(|d| d.storage.extent) else {
            return;
        };
        let Some(dst_extent) = self.store.get(dst_id).map(|d| d.storage.extent) else {
            return;
        };
        // #133 step 3 (P4): clamp against each handle's content BOUNDS
        // rather than its raw storage extent, so neither the read nor the
        // write can reach a bordered window's ring. At `bw == 0` the
        // bounds ARE the extents and this is the pre-#133 arithmetic.
        let src_bounds = src_handle.bounds_in(src_extent);
        let dst_bounds = dst_handle.bounds_in(dst_extent);
        // Clamp the transfer so both the source read and the
        // destination write stay in bounds; shift both sides by the
        // same delta.
        let mut sx = src_rect.offset.x;
        let mut sy = src_rect.offset.y;
        let mut dx = dst_pos.x;
        let mut dy = dst_pos.y;
        let mut w = src_rect.extent.width as i32;
        let mut h = src_rect.extent.height as i32;
        let clamp_low = |pos: &mut i32, other: &mut i32, len: &mut i32, floor: i32| {
            if *pos < floor {
                let d = floor - *pos;
                *other += d;
                *len -= d;
                *pos = floor;
            }
        };
        clamp_low(&mut sx, &mut dx, &mut w, src_bounds.offset.x);
        clamp_low(&mut sy, &mut dy, &mut h, src_bounds.offset.y);
        clamp_low(&mut dx, &mut sx, &mut w, dst_bounds.offset.x);
        clamp_low(&mut dy, &mut sy, &mut h, dst_bounds.offset.y);
        w = w
            .min(src_bounds.offset.x + src_bounds.extent.width as i32 - sx)
            .min(dst_bounds.offset.x + dst_bounds.extent.width as i32 - dx);
        h = h
            .min(src_bounds.offset.y + src_bounds.extent.height as i32 - sy)
            .min(dst_bounds.offset.y + dst_bounds.extent.height as i32 - dy);
        if w <= 0 || h <= 0 {
            return;
        }
        let rect = |x: i32, y: i32| ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x, y },
            extent: ash::vk::Extent2D {
                width: w as u32,
                height: h as u32,
            },
        };
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::CopyAreaRop);
        let src_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            src_handle,
            rect(sx, sy),
            depth,
        ) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("render copy_area_rop_cpu: src get_image failed: {e:?}");
                return;
            }
        };
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::CopyAreaRop);
        let mut dst_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            dst_handle.read_back(),
            rect(dx, dy),
            depth,
        ) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("render copy_area_rop_cpu: dst get_image failed: {e:?}");
                return;
            }
        };
        let full_mask = depth_plane_mask(depth);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = read_z_pixmap_pixel(&src_bytes, depth, w as u32, x, y) & full_mask;
                let d = read_z_pixmap_pixel(&dst_bytes, depth, w as u32, x, y) & full_mask;
                let out = apply_gc_function(function, s, d, plane_mask) & full_mask;
                write_z_pixmap_pixel(&mut dst_bytes, depth, w as u32, x, y, out);
            }
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            dst_handle,
            ash::vk::Offset2D { x: dx, y: dy },
            ash::vk::Extent2D {
                width: w as u32,
                height: h as u32,
            },
            &dst_bytes,
            depth,
        ) {
            log::warn!("render copy_area_rop_cpu: put_image failed: {e:?}");
            return;
        }
        self.telemetry.record_paint_submit();
        self.trace_simple(SubmitKind::CopyArea, dst_id, 1);
    }

    /// PutImage with a non-Copy GC function, partial plane-mask, or
    /// a pixmap clip-mask: CPU read-modify-write combining the wire
    /// z-pixmap image with the destination through
    /// `apply_gc_function`. `data` has row stride `data_width`
    /// pixels; the transfer covers `transfer_w`×`transfer_h` pixels
    /// starting at `src_off` within the image. Conformance path only
    /// — real clients put with GXcopy and no bitmap clip.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::kms::render::backend) fn put_image_rop_cpu(
        &mut self,
        dst_handle: Dst,
        dst_pos: ash::vk::Offset2D,
        data_width: u16,
        src_off: (i32, i32),
        transfer_w: u16,
        transfer_h: u16,
        data: &[u8],
        depth: u8,
        function: yserver_core::backend::GcFunction,
        plane_mask: u32,
    ) {
        let dst_id = dst_handle.id();
        let width = data_width;
        let Some(dst_extent) = self.store.get(dst_id).map(|d| d.storage.extent) else {
            return;
        };
        // #133 step 3 (P4): the clamp floor/ceiling is the handle's
        // content BOUNDS, so a rop PutImage whose rect starts left of or
        // above the content origin loses those leading rows/columns —
        // exactly what the GPU path's `clamp_put_rect_to` does — instead
        // of writing the border ring. At `bw == 0` the bounds ARE the
        // storage extent, i.e. the pre-#133 arithmetic.
        let bounds = dst_handle.bounds_in(dst_extent);
        // Clamp to the destination; track the source-image offset of
        // the clamped origin.
        let mut dx = dst_pos.x;
        let mut dy = dst_pos.y;
        let mut sx = src_off.0;
        let mut sy = src_off.1;
        let mut w = i32::from(transfer_w);
        let mut h = i32::from(transfer_h);
        if dx < bounds.offset.x {
            let d = bounds.offset.x - dx;
            sx += d;
            w -= d;
            dx = bounds.offset.x;
        }
        if dy < bounds.offset.y {
            let d = bounds.offset.y - dy;
            sy += d;
            h -= d;
            dy = bounds.offset.y;
        }
        w = w.min(bounds.offset.x + bounds.extent.width as i32 - dx);
        h = h.min(bounds.offset.y + bounds.extent.height as i32 - dy);
        if w <= 0 || h <= 0 {
            return;
        }
        let dst_rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: dx, y: dy },
            extent: ash::vk::Extent2D {
                width: w as u32,
                height: h as u32,
            },
        };
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::PutImageRop);
        let mut dst_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            dst_handle.read_back(),
            dst_rect,
            depth,
        ) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("render put_image_rop_cpu: dst get_image failed: {e:?}");
                return;
            }
        };
        let full_mask = depth_plane_mask(depth);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let s = read_z_pixmap_pixel(
                    data,
                    depth,
                    u32::from(width),
                    x + sx as usize,
                    y + sy as usize,
                ) & full_mask;
                let d = read_z_pixmap_pixel(&dst_bytes, depth, w as u32, x, y) & full_mask;
                let out = apply_gc_function(function, s, d, plane_mask) & full_mask;
                write_z_pixmap_pixel(&mut dst_bytes, depth, w as u32, x, y, out);
            }
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            dst_handle,
            dst_rect.offset,
            dst_rect.extent,
            &dst_bytes,
            depth,
        ) {
            log::warn!("render put_image_rop_cpu: put_image failed: {e:?}");
        }
    }

    /// Fill `rects` on `id`, honouring `KmsCore.current_fill`. Used
    /// by the filled-shape ops (`PolyFillRectangle`, `PolyFillArc`,
    /// `FillPoly`, `FillRectangle`); stroke ops keep using
    /// [`fill_solid_rects`] because X11 strokes are always solid
    /// foreground regardless of GC fill-style.
    ///
    /// `Solid` stays on the fast GPU path. The patterned styles
    /// (`Tiled`, `Stippled`, `OpaqueStippled`) use a CPU read/modify/write
    /// fallback so X11 function, plane-mask, tile/stipple origin, and
    /// opaque-background semantics all stay exact.
    pub(in crate::kms::render::backend) fn fill_rects_honoring_fill_state(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        target: PaintTarget,
        fg: u32,
        rects: &[Rectangle16],
    ) {
        use yserver_core::backend::{FillState, GcFunction};
        if rects.is_empty() {
            return;
        }
        // Legacy root-overlay idiom: reroute reversible root+IncludeInferiors
        // SOLID fills to the compose-time front-buffer overlay instead of the
        // (occluded) root backing. Gate FIRST — before the inferior-tree walk —
        // so the overlay path pays for no wasted child collection. Patterned
        // (tiled/stippled) fills are excluded (the overlay stores a single
        // XOR/invert color). For IncludeInferiors,
        // `clip_fill_rects_by_subwindow_mode` is pass-through, so the raw rects
        // equal what the backing path would have captured.
        if matches!(self.core.current_fill, FillState::Solid)
            && self.should_route_root_overlay(host_xid, origin)
        {
            self.capture_root_overlay(origin, fg, rects);
            return;
        }
        let include_inferiors = matches!(
            self.core.current_subwindow_mode,
            yserver_core::backend::SubwindowMode::IncludeInferiors,
        ) && (self.windows.contains_key(&host_xid)
            || host_xid == self.core.window_id);
        let inferior_work = if include_inferiors {
            self.collect_fill_rects_for_inferiors(host_xid, rects)
        } else {
            Vec::new()
        };
        let function = self.core.current_function;
        if matches!(function, GcFunction::NoOp) {
            return;
        }
        let rects = self.clip_fill_rects_by_subwindow_mode(host_xid, rects);
        if rects.is_empty() {
            return;
        }
        let fill = self.core.current_fill.clone();
        match fill {
            FillState::Solid => {
                self.fill_solid_rects(target, fg, &rects);
            }
            FillState::Tiled { .. }
            | FillState::Stippled { .. }
            | FillState::OpaqueStippled { .. } => {
                self.fill_pattern_rects_cpu_fallback(target, fg, &rects, &fill);
            }
        }
        for (child_xid, child_rects) in inferior_work {
            if child_rects.is_empty() {
                continue;
            }
            let Some(child_target) = self.resolve_paint_target(child_xid) else {
                continue;
            };
            // The inferior's own clip in the shared backing, not its
            // ancestor's: higher siblings over it, its bounding shape.
            let child_rects = match self.shared_backing_draw_clip(child_xid, &child_target) {
                Some(keep) => clip_rects16(&child_rects, &keep),
                None => child_rects,
            };
            match &fill {
                FillState::Solid => self.fill_solid_rects(child_target, fg, &child_rects),
                FillState::Tiled { .. }
                | FillState::Stippled { .. }
                | FillState::OpaqueStippled { .. } => {
                    self.fill_pattern_rects_cpu_fallback(child_target, fg, &child_rects, &fill);
                }
            }
        }
    }

    fn fill_pattern_rects_cpu_fallback(
        &mut self,
        target: PaintTarget,
        fg: u32,
        rects: &[Rectangle16],
        fill: &yserver_core::backend::FillState,
    ) {
        use yserver_core::backend::FillState;

        if rects.is_empty() {
            return;
        }
        let id = target.backing_id();
        let Some((depth, extent)) = self.store.get(id).map(|d| (d.depth, d.storage.extent)) else {
            return;
        };
        // #133 step 3 (P4): the readback and write-back below span the
        // WHOLE storage (the row geometry the pixel loop indexes with) and
        // are therefore PRIVILEGED; the region actually modified is the
        // content-clipped rect list. Identity at `bw == 0`.
        let rects = &target.clip_local_rects(rects)[..];
        if rects.is_empty() {
            return;
        }
        let full_mask = depth_plane_mask(depth);
        let plane_mask = self.core.current_plane_mask & full_mask;
        if plane_mask == 0 {
            return;
        }
        let dst_rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D::default(),
            extent,
        };
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::CpuFallbackPattern);
        let mut dst_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            Src::server_internal(id),
            dst_rect,
            depth,
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("render fill_pattern_rects_cpu_fallback: get_image failed: {e:?}");
                return;
            }
        };

        struct PatternSource {
            depth: u8,
            width: u32,
            height: u32,
            bytes: Vec<u8>,
            origin: (i16, i16),
        }

        let pattern_source = match fill {
            FillState::Tiled { pixmap, origin }
            | FillState::Stippled { pixmap, origin }
            | FillState::OpaqueStippled { pixmap, origin } => {
                if let Some(fresh) = self.read_fill_pattern_cache(pixmap.as_raw(), *origin) {
                    self.fill_pattern_cache = Some(fresh);
                } else if let Some(cache) = self.fill_pattern_cache.as_mut() {
                    if cache.pixmap_xid == pixmap.as_raw() {
                        cache.origin = *origin;
                    } else {
                        self.fill_pattern_cache = None;
                    }
                } else {
                    self.fill_pattern_cache = None;
                }
                let Some(cache) = self.fill_pattern_cache.as_ref() else {
                    self.fill_solid_rects(target, fg, rects);
                    return;
                };
                if cache.pixmap_xid != pixmap.as_raw() {
                    self.fill_solid_rects(target, fg, rects);
                    return;
                }
                PatternSource {
                    depth: cache.depth,
                    width: cache.width,
                    height: cache.height,
                    bytes: cache.bytes.clone(),
                    origin: cache.origin,
                }
            }
            FillState::Solid => {
                self.fill_solid_rects(target, fg, rects);
                return;
            }
        };

        let function = self.core.current_function;
        let bg = self.core.current_background & full_mask;
        let fg = fg & full_mask;
        let (dx, dy) = target.offset();
        for r in rects {
            let local_x0 = i32::from(r.x);
            let local_y0 = i32::from(r.y);
            let local_x1 = local_x0.saturating_add(i32::from(r.width));
            let local_y1 = local_y0.saturating_add(i32::from(r.height));
            for local_y in local_y0..local_y1 {
                for local_x in local_x0..local_x1 {
                    let storage_x = local_x + dx;
                    let storage_y = local_y + dy;
                    if storage_x < 0
                        || storage_y < 0
                        || storage_x >= extent.width as i32
                        || storage_y >= extent.height as i32
                    {
                        continue;
                    }
                    let dst = read_z_pixmap_pixel(
                        &dst_bytes,
                        depth,
                        extent.width,
                        storage_x as usize,
                        storage_y as usize,
                    ) & full_mask;
                    let out = match fill {
                        FillState::Tiled { .. } => {
                            let sx = (local_x - i32::from(pattern_source.origin.0))
                                .rem_euclid(pattern_source.width as i32)
                                as usize;
                            let sy = (local_y - i32::from(pattern_source.origin.1))
                                .rem_euclid(pattern_source.height as i32)
                                as usize;
                            let src = read_z_pixmap_pixel(
                                &pattern_source.bytes,
                                pattern_source.depth,
                                pattern_source.width,
                                sx,
                                sy,
                            ) & full_mask;
                            apply_gc_function(function, src, dst, plane_mask) & full_mask
                        }
                        FillState::Stippled { .. } | FillState::OpaqueStippled { .. } => {
                            let sx = (local_x - i32::from(pattern_source.origin.0))
                                .rem_euclid(pattern_source.width as i32)
                                as usize;
                            let sy = (local_y - i32::from(pattern_source.origin.1))
                                .rem_euclid(pattern_source.height as i32)
                                as usize;
                            let bit = read_z_pixmap_pixel(
                                &pattern_source.bytes,
                                pattern_source.depth,
                                pattern_source.width,
                                sx,
                                sy,
                            ) != 0;
                            let src = if bit {
                                Some(fg)
                            } else if matches!(fill, FillState::OpaqueStippled { .. }) {
                                Some(bg)
                            } else {
                                None
                            };
                            match src {
                                Some(src) => {
                                    apply_gc_function(function, src, dst, plane_mask) & full_mask
                                }
                                None => dst,
                            }
                        }
                        FillState::Solid => dst,
                    };
                    write_z_pixmap_pixel(
                        &mut dst_bytes,
                        depth,
                        extent.width,
                        storage_x as usize,
                        storage_y as usize,
                        out,
                    );
                }
            }
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            Dst::server_internal(id),
            ash::vk::Offset2D::default(),
            extent,
            &dst_bytes,
            depth,
        ) {
            log::warn!("render fill_pattern_rects_cpu_fallback: put_image failed: {e:?}");
            return;
        }
        self.telemetry.record_paint_submit();
        self.trace_simple(
            if matches!(function, yserver_core::backend::GcFunction::Copy) {
                SubmitKind::FillBatch
            } else {
                SubmitKind::LogicFill
            },
            id,
            u32::try_from(rects.len()).unwrap_or(u32::MAX),
        );
    }

    /// Tile fill via `engine.render_composite` (Stage 3f.3). Returns
    /// `true` iff the call submitted; `false` if the tile isn't
    /// usable (unknown xid, self-tile aliasing, non-BGRA8 tile
    /// format), in which case the caller falls back to solid.
    ///
    /// Stage 4a: `dst` carries the resolved DrawableId + offset.
    /// Dst-space rect origins are shifted by `dst.offset` to land
    /// in backing coords; `src_x/src_y` stay window-local because
    /// they're a `(dst - tile_origin)` difference that doesn't
    /// depend on the absolute frame.
    #[allow(dead_code)]
    fn try_tiled_fill(
        &mut self,
        dst: PaintTarget,
        tile_xid: u32,
        ox: i16,
        oy: i16,
        rects: &[Rectangle16],
    ) -> bool {
        use crate::kms::{
            render::engine::{ResolvedSource, SourceDrawable},
            vk::ops::render::CompositeRect,
        };
        if rects.is_empty() {
            return true;
        }
        let Some(tile_id) = self.store.lookup(tile_xid) else {
            log::debug!("render try_tiled_fill: tile 0x{tile_xid:x} not in store");
            return false;
        };
        if dst.backing_id() == tile_id {
            // Self-tile would alias src + dst inside render_composite.
            return false;
        }
        let tile_format = self.store.get(tile_id).map(|d| d.storage.format);
        if tile_format != Some(ash::vk::Format::B8G8R8A8_UNORM) {
            log::debug!(
                "render try_tiled_fill: tile 0x{tile_xid:x} format {tile_format:?} not BGRA8"
            );
            return false;
        }
        let (dx, dy) = dst.offset();
        // Build per-rect CompositeRects in dst space with
        // `src_origin = dst - tile_origin` so the shader's
        // `src_origin + dst_offset` lands on the right tile pixel.
        let composite_rects: Vec<CompositeRect> = rects
            .iter()
            .filter_map(|r| {
                if r.width == 0 || r.height == 0 {
                    return None;
                }
                Some(CompositeRect {
                    src_x: i32::from(r.x) - i32::from(ox),
                    src_y: i32::from(r.y) - i32::from(oy),
                    mask_x: 0,
                    mask_y: 0,
                    dst_x: i32::from(r.x) + dx,
                    dst_y: i32::from(r.y) + dy,
                    width: u32::from(r.width),
                    height: u32::from(r.height),
                })
            })
            .collect();
        if composite_rects.is_empty() {
            return true;
        }
        // Op `Src` (1) — tile fill replaces the destination.
        const OP_SRC: u8 = 1;
        let composite_result = self.engine.render_composite(
            &mut self.store,
            &mut self.platform,
            OP_SRC,
            // GC tiles are pixmaps by protocol — whole storage.
            ResolvedSource::Drawable(SourceDrawable::whole(tile_id)),
            ResolvedSource::None,
            dst.dst(),
            &composite_rects,
            None, // GC clip already applied by caller
            Repeat::Normal,
            Repeat::None,
            None,
            None,
            false,
            // Audit #4: synthesized tile-fill draw, no Picture
            // context. Engine falls back to depth heuristic.
            0,
            0,
            0,
        );
        self.sync_descriptor_pool_telemetry();
        match composite_result {
            Ok(s) => {
                if s.recorded_draws > 0 && !s.deferred_to_batch {
                    self.telemetry.record_paint_submit();
                    self.trace_render(
                        SubmitKind::RenderComposite,
                        dst.backing_id(),
                        s.recorded_draws,
                        OP_SRC,
                        SrcClass::Direct,
                        None,
                        SubmitFlags {
                            readback: s.used_dst_readback,
                            alias: s.used_src_alias_scratch,
                            zero_draws: false,
                            upload: false,
                        },
                    );
                }
                true
            }
            Err(e) => {
                log::warn!("render try_tiled_fill: render_composite failed: {e:?}");
                false
            }
        }
    }
}

/// The bounding box of the non-empty `rects`, or `None` when there are none.
pub(in crate::kms::render::backend) fn vk_rects_bbox(
    rects: &[ash::vk::Rect2D],
) -> Option<ash::vk::Rect2D> {
    let mut it = rects
        .iter()
        .filter(|r| r.extent.width > 0 && r.extent.height > 0);
    let first = it.next()?;
    let (mut x0, mut y0) = (first.offset.x, first.offset.y);
    let (mut x1, mut y1) = (
        x0 + first.extent.width as i32,
        y0 + first.extent.height as i32,
    );
    for r in it {
        x0 = x0.min(r.offset.x);
        y0 = y0.min(r.offset.y);
        x1 = x1.max(r.offset.x + r.extent.width as i32);
        y1 = y1.max(r.offset.y + r.extent.height as i32);
    }
    Some(ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: x0, y: y0 },
        extent: ash::vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
    })
}

/// Copy `piece` (same space as both images) from `src`, a tightly packed
/// 4-byte-per-pixel image of `src_rect`, into `dst`, one of `dst_rect`.
pub(in crate::kms::render::backend) fn blit_rows_4bpp(
    src: &[u8],
    src_rect: ash::vk::Rect2D,
    dst: &mut [u8],
    dst_rect: ash::vk::Rect2D,
    piece: ash::vk::Rect2D,
) {
    let clip = |a: ash::vk::Rect2D, b: ash::vk::Rect2D| {
        intersect_rect_with_clip(a, &[b]).into_iter().next()
    };
    let Some(piece) = clip(piece, src_rect).and_then(|p| clip(p, dst_rect)) else {
        return;
    };
    let row = piece.extent.width as usize * 4;
    let (src_stride, dst_stride) = (
        src_rect.extent.width as usize * 4,
        dst_rect.extent.width as usize * 4,
    );
    for line in 0..piece.extent.height as i32 {
        let y = piece.offset.y + line;
        let s = (y - src_rect.offset.y) as usize * src_stride
            + (piece.offset.x - src_rect.offset.x) as usize * 4;
        let d = (y - dst_rect.offset.y) as usize * dst_stride
            + (piece.offset.x - dst_rect.offset.x) as usize * 4;
        if let (Some(from), Some(to)) = (src.get(s..s + row), dst.get_mut(d..d + row)) {
            to.copy_from_slice(from);
        }
    }
}

/// The bounding box of the non-empty `rects`, or `None` when there are none.
pub(in crate::kms::render::backend) fn rects16_bbox(
    rects: &[Rectangle16],
) -> Option<ash::vk::Rect2D> {
    let mut it = rects.iter().filter(|r| r.width > 0 && r.height > 0);
    let first = it.next()?;
    let (mut x0, mut y0) = (i32::from(first.x), i32::from(first.y));
    let (mut x1, mut y1) = (x0 + i32::from(first.width), y0 + i32::from(first.height));
    for r in it {
        x0 = x0.min(i32::from(r.x));
        y0 = y0.min(i32::from(r.y));
        x1 = x1.max(i32::from(r.x) + i32::from(r.width));
        y1 = y1.max(i32::from(r.y) + i32::from(r.height));
    }
    Some(ash::vk::Rect2D {
        offset: ash::vk::Offset2D { x: x0, y: y0 },
        extent: ash::vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
    })
}

/// Resolve the drawable depth for a new subwindow. `CopyFromParent`
/// inherits the parent window's depth; only the root / untracked
/// fallback defaults to 24.
/// #133 step 3 round 4 — the wire byte count for a `width x height`
/// ZPixmap image at `depth`: rows padded to 32 bits, mirroring Xorg's
/// `PixmapBytePad(width, depth) * height` (`dix/dispatch.c:2227-2228`).
/// This is the length a `GetImage` reply must always carry, whatever a
/// read's bounds allowed.
pub(in crate::kms::render::backend) fn wire_image_len(depth: u8, width: u32, height: u32) -> usize {
    let bits_per_row = match depth {
        1 => width,
        4 => 4 * width,
        8 => 8 * width,
        _ => 32 * width,
    };
    let row_bytes = (bits_per_row.div_ceil(32) * 4) as usize;
    row_bytes * height as usize
}

/// Wrap raw GetImage pixel bytes into a full X11 GetImage reply
/// (32-byte header + pixels). `sequence` and `visual` are patched in
/// by the handler (`process_request.rs:handle_get_image`); this
/// helper fills the rest. Mirrors v1's
/// `KmsBackend::get_image` (kms/backend.rs:10400-10420) byte-for-byte
/// so the handler's expectations carry across both backends.
pub(in crate::kms::render::backend) fn wrap_get_image_reply(
    depth: u8,
    pixel_bytes: Vec<u8>,
) -> Vec<u8> {
    let pixel_len = pixel_bytes.len();
    let mut out = Vec::with_capacity(32 + pixel_len);
    out.push(1); // [0]: Reply indicator
    out.push(depth); // [1]: depth
    out.extend_from_slice(&[0u8; 2]); // [2..4]: sequence (patched by handler)
    // [4..8]: reply length in u32 units. Rows are already
    // 4-byte aligned for the depths we support (1/8/24/32 — see
    // `pack_from_storage`), so this is `pixel_len / 4`.
    let reply_length_units = u32::try_from(pixel_len / 4).unwrap_or(u32::MAX);
    out.extend_from_slice(&reply_length_units.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // [8..12]: visual (patched by handler)
    out.extend_from_slice(&[0u8; 20]); // [12..32]: padding
    debug_assert_eq!(out.len(), 32);
    out.extend_from_slice(&pixel_bytes);
    out
}

/// All-planes mask for a drawable of `depth` (1 ≤ depth ≤ 32).
pub(in crate::kms::render::backend) fn depth_plane_mask(depth: u8) -> u32 {
    if depth >= 32 {
        u32::MAX
    } else {
        (1u32 << depth) - 1
    }
}

/// Branch selector for the `ClipState::Pixmap` CopyArea path: a clip-masked
/// copy can take the fast GPU per-run blit ONLY when the rop is plain GXcopy
/// and the plane-mask is full — then each clip-mask run is a straight
/// rectangular copy with no read-modify-write. Any other rop / a partial
/// plane-mask must use the CPU `copy_area_rop_cpu` path. gkrellm draws
/// GXcopy+full-mask clip-masked copies, which is why routing those to the GPU
/// removed the per-run readback stall (2026-06-20).
pub(in crate::kms::render::backend) fn copy_area_clip_gpu_eligible(
    function: yserver_core::backend::GcFunction,
    plane_mask: u32,
    full_mask: u32,
) -> bool {
    matches!(function, yserver_core::backend::GcFunction::Copy) && plane_mask == full_mask
}

/// In-scope predicate for routing a clip-masked CopyArea through the GPU
/// `masked_copy_area` path (Task 14). Requires the existing GXcopy +
/// full-plane-mask eligibility AND a destination depth whose format passed
/// the Task 9 byte-exactness gate (32 / 24 / 8). Out-of-scope cases
/// (non-Copy rop, partial plane mask, excluded depth) fall through to the
/// existing run-based CPU/GPU path unchanged.
pub(in crate::kms::render::backend) fn copy_area_masked_blit_eligible(
    function: yserver_core::backend::GcFunction,
    plane_mask: u32,
    full_mask: u32,
    dst_depth: u8,
) -> bool {
    copy_area_clip_gpu_eligible(function, plane_mask, full_mask) && matches!(dst_depth, 32 | 24 | 8)
}

/// Apply a ZPixmap `plane_mask` in place to wire-format pixel rows as
/// produced by `pack_from_storage`: depth 1 = 1bpp bitmap rows, depth 8
/// = byte rows padded to 4, depth 24/32 = BGRA u32 LE. Per the X11
/// spec, GetImage ZPixmap returns zero bits in all planes not in
/// `plane_mask` (the full pixel grid is still transmitted). `mask` is
/// already truncated to the drawable depth, so for depth 24 the X byte
/// gets cleared too — its content is undefined on the wire.
pub(in crate::kms::render::backend) fn apply_z_plane_mask(bytes: &mut [u8], depth: u8, mask: u32) {
    match depth {
        1 => {
            if mask & 1 == 0 {
                bytes.fill(0);
            }
        }
        4 | 8 => {
            let m = (mask & 0xff) as u8;
            if depth == 4 {
                let m = m & 0x0f;
                for b in bytes.iter_mut() {
                    *b = (*b & 0x0f & m) | (((*b >> 4) & m) << 4);
                }
            } else {
                for b in bytes.iter_mut() {
                    *b &= m;
                }
            }
        }
        24 | 32 => {
            for px in bytes.chunks_exact_mut(4) {
                let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]) & mask;
                px.copy_from_slice(&v.to_le_bytes());
            }
        }
        // Depths the engine can't read back never get here (the
        // engine already errored and the handler sent the fallback).
        _ => bytes.fill(0),
    }
}

fn z_pixmap_row_stride(depth: u8, width: u32) -> usize {
    match depth {
        1 => width.div_ceil(32) as usize * 4,
        4 => width.div_ceil(8) as usize * 4,
        8 => (width as usize + 3) & !3,
        24 | 32 => width as usize * 4,
        _ => 0,
    }
}

pub(in crate::kms::render::backend) fn read_z_pixmap_pixel(
    bytes: &[u8],
    depth: u8,
    width: u32,
    x: usize,
    y: usize,
) -> u32 {
    let stride = z_pixmap_row_stride(depth, width);
    match depth {
        1 => {
            let byte = bytes[y * stride + x / 8];
            u32::from((byte >> (x % 8)) & 1)
        }
        4 => {
            let byte = bytes[y * stride + x / 2];
            u32::from(if x.is_multiple_of(2) {
                byte & 0x0f
            } else {
                (byte >> 4) & 0x0f
            })
        }
        8 => u32::from(bytes[y * stride + x]),
        24 | 32 => {
            let off = y * stride + x * 4;
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        }
        _ => 0,
    }
}

pub(in crate::kms::render::backend) fn write_z_pixmap_pixel(
    bytes: &mut [u8],
    depth: u8,
    width: u32,
    x: usize,
    y: usize,
    value: u32,
) {
    let stride = z_pixmap_row_stride(depth, width);
    match depth {
        1 => {
            let byte = &mut bytes[y * stride + x / 8];
            let bit = 1u8 << (x % 8);
            if value & 1 != 0 {
                *byte |= bit;
            } else {
                *byte &= !bit;
            }
        }
        4 => {
            let byte = &mut bytes[y * stride + x / 2];
            let nibble = (value & 0x0f) as u8;
            if x.is_multiple_of(2) {
                *byte = (*byte & 0xf0) | nibble;
            } else {
                *byte = (*byte & 0x0f) | (nibble << 4);
            }
        }
        8 => bytes[y * stride + x] = value as u8,
        24 | 32 => {
            let off = y * stride + x * 4;
            bytes[off..off + 4].copy_from_slice(&value.to_le_bytes());
        }
        _ => {}
    }
}

pub(in crate::kms::render::backend) fn apply_gc_function(
    function: yserver_core::backend::GcFunction,
    src: u32,
    dst: u32,
    mask: u32,
) -> u32 {
    use yserver_core::backend::GcFunction;
    let op = match function {
        GcFunction::Clear => 0,
        GcFunction::And => src & dst,
        GcFunction::AndReverse => src & !dst,
        GcFunction::Copy => src,
        GcFunction::AndInverted => !src & dst,
        GcFunction::NoOp => dst,
        GcFunction::Xor => src ^ dst,
        GcFunction::Or => src | dst,
        GcFunction::Nor => !(src | dst),
        GcFunction::Equiv => !(src ^ dst),
        GcFunction::Invert => !dst,
        GcFunction::OrReverse => src | !dst,
        GcFunction::CopyInverted => !src,
        GcFunction::OrInverted => !src | dst,
        GcFunction::Nand => !(src & dst),
        GcFunction::Set => u32::MAX,
    };
    (op & mask) | (dst & !mask)
}

/// Repack Z-layout wire bytes (per `pack_from_storage`) into XYPixmap
/// wire format: one 1-bit plane per set bit in `mask`, most-significant
/// plane first (X11 §GetImage), scanlines padded to 32 bits, LSBFirst
/// bit order matching the advertised bitmap-format-bit-order (and the
/// depth-1 packing in `pack_from_storage`).
pub(in crate::kms::render::backend) fn z_to_xy_planes(
    z: &[u8],
    w: u32,
    h: u32,
    depth: u8,
    mask: u32,
) -> Vec<u8> {
    let w_us = w as usize;
    let h_us = h as usize;
    let out_stride = w.div_ceil(32) as usize * 4;
    let n_planes = mask.count_ones() as usize;
    let mut out = vec![0u8; out_stride * h_us * n_planes];
    if w_us == 0 || h_us == 0 || n_planes == 0 {
        return out;
    }
    let pixel = |x: usize, y: usize| -> u32 {
        match depth {
            1 => {
                // Already bitmap rows padded to 32 bits.
                let byte = z[y * out_stride + x / 8];
                u32::from((byte >> (x % 8)) & 1)
            }
            4 => {
                let stride = w.div_ceil(8) as usize * 4;
                let byte = z[y * stride + x / 2];
                u32::from(if x.is_multiple_of(2) {
                    byte & 0x0f
                } else {
                    (byte >> 4) & 0x0f
                })
            }
            8 => {
                // Byte rows padded to 4 bytes.
                let stride = (w_us + 3) & !3;
                u32::from(z[y * stride + x])
            }
            // 24/32: tightly packed BGRA u32 LE.
            _ => {
                let off = (y * w_us + x) * 4;
                u32::from_le_bytes([z[off], z[off + 1], z[off + 2], z[off + 3]])
            }
        }
    };
    let mut plane_base = 0;
    for p in (0..32).rev().filter(|p| mask & (1 << p) != 0) {
        for y in 0..h_us {
            let row = plane_base + y * out_stride;
            for x in 0..w_us {
                if (pixel(x, y) >> p) & 1 != 0 {
                    out[row + x / 8] |= 1 << (x % 8);
                }
            }
        }
        plane_base += out_stride * h_us;
    }
    out
}

pub(in crate::kms::render::backend) fn depth_for_visual(
    visual: HostSubwindowVisual,
    parent_depth: Option<u8>,
) -> u8 {
    match visual {
        HostSubwindowVisual::CopyFromParent => parent_depth.unwrap_or(24),
        HostSubwindowVisual::DepthOnly { depth } => {
            if depth == 0 {
                parent_depth.unwrap_or(24)
            } else {
                depth
            }
        }
        HostSubwindowVisual::Explicit { depth, .. } => {
            if depth == 0 {
                parent_depth.unwrap_or(24)
            } else {
                depth
            }
        }
    }
}

pub(in crate::kms::render::backend) fn compute_copy_area_dst_rects(
    dst_rect: ash::vk::Rect2D,
    child_rects: &[ash::vk::Rect2D],
) -> Vec<ash::vk::Rect2D> {
    if dst_rect.extent.width == 0 || dst_rect.extent.height == 0 {
        return Vec::new();
    }
    let mut current = vec![dst_rect];
    for child in child_rects {
        let mut next = Vec::new();
        for r in current {
            next.extend(subtract_one_rect_clip(r, *child));
        }
        current = next;
        if current.is_empty() {
            return current;
        }
    }
    current
}
