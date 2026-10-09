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

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_draw_set_gc_fill_solid(
        &mut self,
        _origin: Option<OriginContext>,
    ) -> io::Result<()> {
        self.core.current_fill = FillState::Solid;
        self.fill_pattern_cache = None;
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_set_gc_fill_tiled(
        &mut self,
        _origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()> {
        // Stage 3f.3: store the FillState::Tiled record so subsequent
        // fill paths route through the tiled-fill RENDER composite.
        // The dispatcher also pushes the same state via
        // `apply_fill_state` before every fill op, so this entry
        // point is mostly used by ynest's host-X11 flow; preserving
        // both keeps the Backend trait surface uniform.
        let Some(handle) = PixmapHandle::from_raw(host_pixmap) else {
            self.core.current_fill = FillState::Solid;
            self.fill_pattern_cache = None;
            return Ok(());
        };
        self.core.current_fill = FillState::Tiled {
            pixmap: handle,
            origin: (tile_x_origin, tile_y_origin),
        };
        self.fill_pattern_cache =
            self.read_fill_pattern_cache(host_pixmap, (tile_x_origin, tile_y_origin));
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_apply_fill_state(
        &mut self,
        _origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()> {
        self.core.current_fill = fill.clone();
        match fill {
            FillState::Tiled { pixmap, origin }
            | FillState::Stippled { pixmap, origin }
            | FillState::OpaqueStippled { pixmap, origin } => {
                let xid = pixmap.as_raw();
                if let Some(fresh) = self.read_fill_pattern_cache(xid, *origin) {
                    self.fill_pattern_cache = Some(fresh);
                } else if let Some(cache) = self.fill_pattern_cache.as_mut() {
                    if cache.pixmap_xid == xid {
                        cache.origin = *origin;
                    } else {
                        self.fill_pattern_cache = None;
                    }
                } else {
                    self.fill_pattern_cache = None;
                }
            }
            FillState::Solid => {
                self.fill_pattern_cache = None;
            }
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_apply_draw_state(
        &mut self,
        _origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()> {
        if let Some(font) = state.font {
            self.core.current_font = Some(font.as_raw());
        }
        self.core.current_function = state.function;
        self.core.current_plane_mask = state.plane_mask;
        self.core.current_foreground = state.foreground;
        self.core.current_background = state.background;
        self.core.current_fill = state.fill.clone();
        self.core.current_clip = state.clip.clone();
        match &state.fill {
            FillState::Tiled { pixmap, origin }
            | FillState::Stippled { pixmap, origin }
            | FillState::OpaqueStippled { pixmap, origin } => {
                let xid = pixmap.as_raw();
                if let Some(fresh) = self.read_fill_pattern_cache(xid, *origin) {
                    self.fill_pattern_cache = Some(fresh);
                } else if let Some(cache) = self.fill_pattern_cache.as_mut() {
                    if cache.pixmap_xid == xid {
                        cache.origin = *origin;
                    } else {
                        self.fill_pattern_cache = None;
                    }
                } else {
                    self.fill_pattern_cache = None;
                }
            }
            FillState::Solid => {
                self.fill_pattern_cache = None;
            }
        }
        // Stage 4d Manual-redirect fix: drawing through a
        // `ClipByChildren` GC into a window must exclude every
        // mapped child window's area. Capture the mode here so
        // `copy_area` (and any other future op that consults it)
        // can split the destination rect against the child rects.
        self.core.current_subwindow_mode = state.subwindow_mode;
        // Stroke state — consumed by poly_line / poly_segment /
        // poly_rectangle / poly_arc via `kms::render::stroke::stroke_path`.
        self.core.current_line_width = state.line_width;
        self.core.current_line_style = state.line_style;
        self.core.current_cap_style = state.cap_style;
        self.core.current_join_style = state.join_style;
        self.core.current_dashes = state.dashes.clone();
        self.core.current_dash_offset = u16::try_from(state.dash_offset).unwrap_or(0);
        self.core.current_arc_mode = state.arc_mode;
        Ok(())
    }

    // ── Drawing primitives (paint paths) ────────────────────────
    pub(in crate::kms::render::backend) fn backend_draw_copy_area(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.telemetry.record_copy_area_call();
        // IncludeInferiors copies what the source shows, its inferiors'
        // pixels too (`miHandleExposures` exposes only what falls outside
        // `NotClippedByChildren`); a window keeping its own storage holds
        // only its own.
        if src_host_xid != self.core.window_id
            && matches!(
                self.core.current_subwindow_mode,
                yserver_core::backend::SubwindowMode::IncludeInferiors
            )
            && self.windows.contains_key(&src_host_xid)
        {
            let area = vk::Rect2D {
                offset: vk::Offset2D {
                    x: i32::from(src_x),
                    y: i32::from(src_y),
                },
                extent: vk::Extent2D {
                    width: u32::from(width),
                    height: u32::from(height),
                },
            };
            if let Some(scratch) = self.window_inferiors_snapshot(src_host_xid, Some(area)) {
                let result = self.copy_area(
                    _origin,
                    scratch,
                    dst_host_xid,
                    0,
                    0,
                    dst_x,
                    dst_y,
                    width,
                    height,
                );
                let _ = self.free_pixmap(None, scratch);
                return result;
            }
        }
        // Resolve the SOURCE the same way as the destination. A window
        // that is Composite-redirected (or whose ancestor is) has its
        // pixels in the redirect *backing*; its own leaf storage is
        // never painted into. So a window→window self-copy — exactly
        // what a Tk text widget does to scroll its diff pane — MUST
        // read from the backing. Reading the raw leaf storage copies
        // blank/background pixels over the live content, progressively
        // blanking the widget (gitk diff-pane bug).
        // `src_off` is the source window's offset within its backing;
        // it converts the wire window-local src coords into backing
        // coords.
        let Some(src_target) = self.resolve_paint_target(src_host_xid) else {
            if !self.windows.contains_key(&src_host_xid) {
                log::warn!(
                    "render copy_area dropped — src unresolvable: src=0x{src_host_xid:x} \
                     dst=0x{dst_host_xid:x} src_xy=({src_x},{src_y}) dst_xy=({dst_x},{dst_y}) {width}x{height}",
                );
            }
            self.log_unresolved_target(src_host_xid, "copy_area_unknown_xid");
            return Ok(());
        };
        let (src, src_off): (crate::kms::render::store::DrawableId, (i32, i32)) =
            (src_target.backing_id(), src_target.offset());
        // #133 step 3 (P4): the CLIENT handles. `src` above stays for
        // identity comparisons (self-copy, COW routing) and tracing —
        // it cannot paint, since every engine op takes `Src`/`Dst`.
        let src_h = src_target.src();
        // Stage 4a — dst resolves through `resolve_paint_target` so
        // copy_area into a redirected window lands in the backing
        // with the descendant offset applied.
        let Some(dst_target) = self.resolve_paint_target(dst_host_xid) else {
            if !self.windows.contains_key(&dst_host_xid) {
                log::warn!(
                    "render copy_area dropped — dst unresolvable: src=0x{src_host_xid:x} dst=0x{dst_host_xid:x} \
                     src_xy=({src_x},{src_y}) dst_xy=({dst_x},{dst_y}) {width}x{height}",
                );
            }
            self.log_unresolved_target(dst_host_xid, "copy_area_unknown_xid");
            return Ok(());
        };
        // Screenshot fast-path: `CopyArea(src=root, …, IncludeInferiors)` must
        // copy the COMPOSITED desktop (all mapped windows), not the root's own
        // storage (background only). Handled by reading the on-screen scanout,
        // mirroring `get_image`'s root special-case. Returns `true` when it took
        // the copy; `false` falls through to the ordinary root-storage path
        // (non-plain GC state, or no live outputs).
        if self.try_copy_area_root_scanout(
            src_host_xid,
            dst_host_xid,
            &dst_target,
            src_x,
            src_y,
            dst_x,
            dst_y,
            width,
            height,
        )? {
            return Ok(());
        }
        // Stage 4d Manual-redirect fix: split the copy by
        // `subwindow_mode = ClipByChildren` rules when dst is a
        // window. Each surviving sub-rect is in dst-window-local
        // coords; we issue one engine.copy_area per sub-rect,
        // adjusting src offsets by the sub-rect's delta from the
        // original dst_xy. IncludeInferiors (mode=1) keeps the
        // single-rect fast path. Pixmap destinations also keep the
        // fast path (no children to clip against). The non-mask scissor
        // machinery lives in `compute_copy_area_scissors`.
        // Step 1: GC clip intersection (X11 GC `clip-mask` /
        // `SetClipRectangles`). When the GC has explicit clip
        // rectangles, every paint is masked against them first;
        // `ClipState::None` means "no GC clip", and we keep the
        // single-rect fast path. `ClipState::Pixmap` intersects with
        // the rasterized mask (pixel runs via
        // intersect_with_current_clip_live).
        if matches!(
            self.core.current_clip,
            yserver_core::backend::ClipState::Pixmap { .. }
        ) {
            use yserver_core::backend::GcFunction;
            // ── GPU masked-blit route (Task 14) ──────────────────────
            // Insert BEFORE `intersect_with_current_clip_live` — that call
            // materializes CPU clip bytes for the run-based path. For
            // in-scope clip-masked GXcopy copies, route ONE masked draw
            // (mask = the eagerly-populated GPU snapshot,
            // non-mask scissors = compute_copy_area_scissors). Out-of-scope
            // cases fall through to the run-based path unchanged.
            let route_fn = self.core.current_function;
            // The drawable's own depth, not its storage's: a depth-24 child
            // painting into a depth-32 ancestor backing still has 24 planes,
            // and its CPU fallback must force its alpha like any depth-24 write.
            let route_dst_depth = dst_target.x11_depth();
            let route_full_mask = depth_plane_mask(route_dst_depth);
            let route_plane_mask = self.core.current_plane_mask & route_full_mask;
            let route_snapshot = if copy_area_masked_blit_eligible(
                route_fn,
                route_plane_mask,
                route_full_mask,
                route_dst_depth,
            ) {
                self.clip_mask_snapshot
                    .as_ref()
                    .map(|s| (s.id, s.pixmap_xid))
            } else {
                None
            };
            if let Some((sid, snap_xid)) = route_snapshot {
                // COORDINATE SPACES: the masked draw runs in dst BACKING/IMAGE
                // space (gl_FragCoord = image pixel). Mirror the run path's
                // shifts: src by `src_off`, dst by `dst_target.offset()`, clip
                // origin by `dst_target.offset()`, scissors (local) by
                // `dst_target.offset()`. `src_off` and `dst_target.offset()` are
                // both `(i32, i32)` tuples accessed via `.0`/`.1`.
                let (sox, soy) = src_off;
                let (tox, toy) = dst_target.offset();
                let scissors: Vec<ash::vk::Rect2D> = self
                    .compute_copy_area_scissors(
                        dst_host_xid,
                        &dst_target,
                        dst_x,
                        dst_y,
                        width,
                        height,
                    )
                    .into_iter()
                    .map(|r| ash::vk::Rect2D {
                        offset: ash::vk::Offset2D {
                            x: r.offset.x + tox,
                            y: r.offset.y + toy,
                        },
                        extent: r.extent,
                    })
                    .collect();
                if scissors.is_empty() {
                    // Fully clipped away — spec-correct no-op.
                    return Ok(());
                }

                // Single refresh mechanism (Task 11/13): re-snapshot only when
                // the live mask changed since the snapshot AND the source
                // pixmap is still alive. If freed, the snapshot (populated at
                // install) is authoritative — retain-after-free.
                if let Some(did) = self.store.lookup(snap_xid) {
                    let live_ver = self.store.get(did).map(|d| d.content_version);
                    if let Some(v) = live_ver
                        && self.engine.clip_snapshot_version(sid) != live_ver
                    {
                        self.engine
                            .refresh_clip_snapshot(&mut self.store, &mut self.platform, sid, did, v)
                            .map_err(|e| {
                                io::Error::other(format!("refresh_clip_snapshot: {e:?}"))
                            })?;
                    }
                }

                let (origin_x, origin_y) = match &self.core.current_clip {
                    yserver_core::backend::ClipState::Pixmap { origin, .. } => *origin,
                    _ => (0, 0),
                };
                let mask = crate::kms::render::engine::MaskedCopyMask {
                    image: self.engine.clip_snapshot_image(sid).unwrap(),
                    view: self.engine.clip_snapshot_view(sid).unwrap(),
                    old_layout: self.engine.clip_snapshot_layout(sid).unwrap(),
                    extent: self.engine.clip_snapshot_extent(sid).unwrap(),
                    // Clip origin shifted into image space (see COORDINATE
                    // SPACES): frag mask_texel = image_pixel - clip_origin.
                    clip_origin: [i32::from(origin_x) + tox, i32::from(origin_y) + toy],
                    snapshot_id: Some(sid),
                };
                let dst = dst_target.dst();
                // Task 15: count the single masked draw. This path issues ONE
                // masked blit (no per-sub-rect fan-out → does NOT call
                // record_copy_area_gpu_subrect_at(true)) and reads the clip from
                // the eagerly-populated GPU snapshot (no per-copy
                // read_clip_mask_bytes → does NOT bump get_image_by_site[ClipMask]).
                self.telemetry.record_copy_area_masked_draw();
                self.engine
                    .masked_copy_area(
                        &mut self.store,
                        &mut self.platform,
                        src_h,
                        dst,
                        ash::vk::Offset2D {
                            x: i32::from(src_x) + sox,
                            y: i32::from(src_y) + soy,
                        },
                        ash::vk::Offset2D {
                            x: i32::from(dst_x) + tox,
                            y: i32::from(dst_y) + toy,
                        },
                        ash::vk::Extent2D {
                            width: width.into(),
                            height: height.into(),
                        },
                        mask,
                        &scissors,
                    )
                    .map_err(|e| io::Error::other(format!("masked_copy_area: {e:?}")))?;
                // Every pixel of a depth-24 child's area is opaque in a
                // depth-32 backing, so stamping the whole scissor (not only
                // the mask's pixels) is exact, not an approximation.
                self.stamp_opaque_alpha_if_shared(dst_target, &scissors);
                self.scene.wake_for_damage();
                // ONE masked draw replaces the run fan-out. Telemetry: Task 15.
                return Ok(());
            }

            let local = Rectangle16 {
                x: dst_x,
                y: dst_y,
                width,
                height,
            };
            // #133 step 3 (P4): content clip first (local space), then the
            // bitmap clip runs. Identity with no border clip.
            let local_clipped = dst_target.clip_local_rects(&[local]);
            let runs = self.intersect_with_current_clip_live(&local_clipped);
            // Mask runs honor function/plane-mask via the CPU path
            // (Copy through apply_gc_function = src — bitwise exact).
            let function = self.core.current_function;
            if matches!(function, GcFunction::NoOp) {
                return Ok(());
            }
            // As `route_dst_depth` above: the drawable's depth, not its storage's.
            let dst_depth = dst_target.x11_depth();
            let full_mask = depth_plane_mask(dst_depth);
            let plane_mask = self.core.current_plane_mask & full_mask;
            if plane_mask == 0 {
                return Ok(());
            }
            // Fast path: GXcopy + full plane-mask is a plain copy clipped
            // to the mask runs — each run is a rectangle, so blit it on the
            // GPU instead of the read-modify-write `copy_area_rop_cpu` path
            // (2 readbacks + per-pixel loop + upload PER RUN). gkrellm
            // draws clip-masked GXcopy at ~250 calls/s × ~6 runs → ~1500
            // CPU runs/s, each stalling on uncached GPU readbacks, which
            // pinned the core loop (2026-06-20 investigation). Only the CPU
            // RMW is needed for genuine non-Copy rops / partial plane masks.
            let gpu_fast = copy_area_clip_gpu_eligible(function, plane_mask, full_mask);
            let routes_to_cow =
                self.cow_id == Some(dst_target.backing_id()) && src != dst_target.backing_id();
            let mut any_gpu = false;
            let mut copied: Vec<ash::vk::Rect2D> = Vec::new();
            for run in runs {
                let sub_src = ash::vk::Rect2D {
                    offset: ash::vk::Offset2D {
                        x: i32::from(src_x) + src_off.0 + (i32::from(run.x) - i32::from(dst_x)),
                        y: i32::from(src_y) + src_off.1 + (i32::from(run.y) - i32::from(dst_y)),
                    },
                    extent: ash::vk::Extent2D {
                        width: u32::from(run.width),
                        height: u32::from(run.height),
                    },
                };
                let dst_pos = ash::vk::Offset2D {
                    x: i32::from(run.x) + dst_target.offset().0,
                    y: i32::from(run.y) + dst_target.offset().1,
                };
                if gpu_fast {
                    self.engine_copy_area_calls = self.engine_copy_area_calls.wrapping_add(1);
                    self.telemetry.record_copy_area_gpu_subrect_at(true);
                    let res = if routes_to_cow {
                        self.engine.cow_copy_area(
                            &mut self.store,
                            &mut self.platform,
                            dst_target.dst(),
                            src_h,
                            sub_src,
                            dst_pos,
                        )
                    } else {
                        self.engine.copy_area(
                            &mut self.store,
                            &mut self.platform,
                            src_h,
                            dst_target.dst(),
                            sub_src,
                            dst_pos,
                        )
                    };
                    if let Err(e) = res {
                        log::warn!(
                            "render copy_area: clip-masked engine.copy_area failed \
                             (src=0x{src_host_xid:x} dst=0x{dst_host_xid:x} run={sub_src:?} \
                             cow_routed={routes_to_cow}): {e:?}",
                        );
                    } else {
                        any_gpu = true;
                        copied.push(ash::vk::Rect2D {
                            offset: dst_pos,
                            extent: sub_src.extent,
                        });
                    }
                } else {
                    self.telemetry.record_copy_area_cpu_pixmap_clip();
                    self.copy_area_rop_cpu(
                        src_h,
                        dst_target.dst(),
                        sub_src,
                        dst_pos,
                        function,
                        plane_mask,
                        dst_depth,
                    );
                }
            }
            self.stamp_opaque_alpha_if_shared(dst_target, &copied);
            if any_gpu && !routes_to_cow {
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::CopyArea, dst_target.backing_id(), 1);
            }
            self.scene.wake_for_damage();
            return Ok(());
        }
        // Non-mask clip machinery (GC clip-rect intersect + ClipByChildren
        // child subtraction + higher-sibling occluder subtraction), in
        // drawable-LOCAL space. Extracted into `compute_copy_area_scissors`
        // and shared with the GPU masked path (Task 14). Behaviour is
        // identical: an empty result (GC clip empty OR fully occluded) is a
        // spec-correct no-op handled by the `sub_rects.is_empty()` guard.
        let sub_rects: Vec<ash::vk::Rect2D> =
            self.compute_copy_area_scissors(dst_host_xid, &dst_target, dst_x, dst_y, width, height);
        if sub_rects.is_empty() {
            // Whole copy is fully clipped away (empty GC clip, or fully
            // covered by mapped children / higher siblings).
            return Ok(());
        }
        // GC function + plane-mask on copies (X11 §CopyArea uses the
        // full rop set). The engine blit is raw GXcopy; anything else
        // (or a partial plane mask) takes the CPU read-modify-write
        // path. NoOp = spec-correct no-op.
        {
            use yserver_core::backend::GcFunction;
            let function = self.core.current_function;
            if matches!(function, GcFunction::NoOp) {
                return Ok(());
            }
            let dst_depth = dst_target.x11_depth();
            let full_mask = depth_plane_mask(dst_depth);
            let plane_mask = self.core.current_plane_mask & full_mask;
            if plane_mask == 0 {
                return Ok(());
            }
            if !matches!(function, GcFunction::Copy) || plane_mask != full_mask {
                for sub in &sub_rects {
                    let sub_src = ash::vk::Rect2D {
                        offset: ash::vk::Offset2D {
                            x: i32::from(src_x) + src_off.0 + (sub.offset.x - i32::from(dst_x)),
                            y: i32::from(src_y) + src_off.1 + (sub.offset.y - i32::from(dst_y)),
                        },
                        extent: sub.extent,
                    };
                    let dst_pos = ash::vk::Offset2D {
                        x: sub.offset.x + dst_target.offset().0,
                        y: sub.offset.y + dst_target.offset().1,
                    };
                    self.telemetry.record_copy_area_cpu_rop();
                    self.copy_area_rop_cpu(
                        src_h,
                        dst_target.dst(),
                        sub_src,
                        dst_pos,
                        function,
                        plane_mask,
                        dst_depth,
                    );
                }
                self.scene.wake_for_damage();
                return Ok(());
            }
        }
        // Stage 5 Task 3 POC: route copy_area to COW through the
        // frame-builder path. Marco's compositor pump is the hot
        // workload (silence trace: 47k of 62k copy_areas target
        // COW). Telemetry for cow-routed copies is deferred.
        let routes_to_cow =
            self.cow_id == Some(dst_target.backing_id()) && src != dst_target.backing_id();

        let mut all_ok = true;
        let mut copied: Vec<ash::vk::Rect2D> = Vec::with_capacity(sub_rects.len());
        for sub in &sub_rects {
            let sub_dst_x = sub.offset.x;
            let sub_dst_y = sub.offset.y;
            // src coords shift by the same delta the dst sub-rect
            // shifted from the original dst_xy, plus the source
            // window's offset within its backing (`src_off`).
            let sub_src_x = i32::from(src_x) + src_off.0 + (sub_dst_x - i32::from(dst_x));
            let sub_src_y = i32::from(src_y) + src_off.1 + (sub_dst_y - i32::from(dst_y));
            let src_sub_rect = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: sub_src_x,
                    y: sub_src_y,
                },
                extent: sub.extent,
            };
            let dst_pos = ash::vk::Offset2D {
                x: sub_dst_x + dst_target.offset().0,
                y: sub_dst_y + dst_target.offset().1,
            };
            self.engine_copy_area_calls = self.engine_copy_area_calls.wrapping_add(1);
            self.telemetry.record_copy_area_gpu_subrect_at(false);
            let res = if routes_to_cow {
                self.engine.cow_copy_area(
                    &mut self.store,
                    &mut self.platform,
                    dst_target.dst(),
                    src_h,
                    src_sub_rect,
                    dst_pos,
                )
            } else {
                self.engine.copy_area(
                    &mut self.store,
                    &mut self.platform,
                    src_h,
                    dst_target.dst(),
                    src_sub_rect,
                    dst_pos,
                )
            };
            if let Err(e) = res {
                log::warn!(
                    "render copy_area: engine.copy_area failed (src=0x{src_host_xid:x} \
                     dst=0x{dst_host_xid:x} sub_rect={sub:?} cow_routed={routes_to_cow}): {e:?}",
                );
                all_ok = false;
            } else {
                copied.push(ash::vk::Rect2D {
                    offset: dst_pos,
                    extent: sub.extent,
                });
            }
        }
        // The PresentPixmap path lands here: a raw image copy that carries
        // the source's X byte into the backing verbatim.
        self.stamp_opaque_alpha_if_shared(dst_target, &copied);
        if all_ok {
            if !routes_to_cow {
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::CopyArea, dst_target.backing_id(), 1);
            }
            // Present Copy into COW/backings must wake the scene
            // compositor immediately; otherwise the damage can sit
            // until an unrelated input event arrives.
            self.scene.wake_for_damage();
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_copy_plane(
        &mut self,
        _origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()> {
        // copy_plane decomposes into bg-first + fg-second
        // `poly_fill_rectangle` calls below; non-`GXcopy` GC.function
        // is honoured by the underlying `fill_solid_rects` →
        // `engine.logic_fill` path landed in Stage 3f.2.
        if width == 0 || height == 0 {
            return Ok(());
        }

        // #133 step 3 (P4) — resolve the SOURCE through
        // `resolve_paint_target` like every other read does, BEFORE
        // touching storage. A raw `store.lookup` was tolerable while
        // storage and logical drawable space coincided; with borders it
        // is neither: it misses COMPOSITE redirect routing (a redirected
        // source's pixels live in the backing, its leaf is stale — the
        // same reason `copy_area` resolves its source) and no
        // hand-reconstructed offset can express nested ancestry. The
        // handle gives all three: the drawable that HOLDS the pixels,
        // the content origin inside it, and the content bounds.
        let Some(src_target) = self.resolve_paint_target(src_host_xid) else {
            log::debug!("render copy_plane gap: src 0x{src_host_xid:x} has no paint target");
            return Ok(());
        };
        let src_id = src_target.backing_id();
        let Some(_dst_id) = self.store.lookup(dst_host_xid) else {
            log::debug!("render copy_plane gap: dst 0x{dst_host_xid:x} not in store");
            return Ok(());
        };

        let src_depth = match self.store.get(src_id) {
            Some(d) => d.depth,
            None => return Ok(()),
        };

        // Read the full src extent via the engine. We pull the
        // whole pixmap once (rather than only `src_rect`) because
        // the wire format's row stride is computed from the
        // pixmap's width; reading a sub-rect would still produce a
        // wire-shaped reply but with a different row stride per
        // pixmap.width. Easier to pull everything, index inside
        // the (src_x, src_y, width, height) window, and let v2's
        // per-op CB amortise the synchronous get_image cost. xfd
        // / xfontsel CopyPlane the entire glyph pixmap each draw
        // anyway, so the "full extent" overhead matches the call
        // pattern.
        let src_extent = match self.store.get(src_id) {
            Some(d) => d.storage.extent,
            None => return Ok(()),
        };
        let src_w = src_extent.width;
        let src_h = src_extent.height;
        if src_w == 0 || src_h == 0 {
            return Ok(());
        }
        // The sampling window comes from the resolved handle: the
        // content origin (`offset()`) plus the wire `(src_x, src_y)`,
        // bounded by the content rect (`content_bounds()`; `None` =
        // the whole storage, which is every pixmap and every
        // `bw == 0` window). The READ itself stays the whole storage
        // and therefore PRIVILEGED — the wire row stride is computed
        // from the drawable width, so a sub-rect read would change the
        // row geometry the indexing loop below depends on. Resolution
        // happens first (above); this is a full-storage read of the
        // ALREADY-RESOLVED drawable, not an unresolved escape hatch.
        let src_bounds = crate::kms::render::engine::resolve_recorded_bounds(
            src_target.content_bounds(),
            src_extent,
        );
        let (src_off_x, src_off_y) = src_target.offset();
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::CopyPlane);
        let src_bytes = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            src_target.server_backing_src(),
            ash::vk::Rect2D {
                offset: ash::vk::Offset2D::default(),
                extent: src_extent,
            },
            src_depth,
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::warn!("render copy_plane: src get_image failed: {e:?}");
                return Ok(());
            }
        };
        self.telemetry.record_one_shot_submit();
        self.trace_simple(SubmitKind::CopyPlaneRb, src_id, 1);

        // Wire row stride for the src depth (matches pack_from_storage).
        let row_bytes: usize = match src_depth {
            1 => src_w.div_ceil(32) as usize * 4,
            4 => src_w.div_ceil(8) as usize * 4,
            8 => (src_w as usize + 3) & !3,
            24 | 32 => src_w as usize * 4,
            _ => {
                log::debug!("render copy_plane gap: src depth {src_depth} unsupported");
                return Ok(());
            }
        };

        // For each (sx, sy) in the requested src window, classify
        // the pixel into foreground / background and emit a 1×1
        // fill rect at the corresponding dst position. Caller
        // saturates over i16 because dst coords are protocol-i16.
        let mut fg_rects: Vec<u8> = Vec::new();
        let mut bg_rects: Vec<u8> = Vec::new();
        // Content window in STORAGE coordinates, straight off the
        // resolved bounds: `[bw, bw + w)` for a bordered window,
        // `[0, extent)` for everything else.
        let sx_lo = src_bounds.offset.x;
        let sy_lo = src_bounds.offset.y;
        let sx_hi = (src_bounds
            .offset
            .x
            .saturating_add_unsigned(src_bounds.extent.width))
        .min(i32::try_from(src_w).unwrap_or(i32::MAX));
        let sy_hi = (src_bounds
            .offset
            .y
            .saturating_add_unsigned(src_bounds.extent.height))
        .min(i32::try_from(src_h).unwrap_or(i32::MAX));
        for row in 0..height {
            let sy = i32::from(src_y)
                .saturating_add(i32::from(row))
                .saturating_add(src_off_y);
            let dy = dst_y.saturating_add(row as i16);
            if sy < sy_lo || sy >= sy_hi {
                continue;
            }
            for col in 0..width {
                let sx = i32::from(src_x)
                    .saturating_add(i32::from(col))
                    .saturating_add(src_off_x);
                let dx = dst_x.saturating_add(col as i16);
                if sx < sx_lo || sx >= sx_hi {
                    continue;
                }
                let pixel: u32 = match src_depth {
                    1 => {
                        // LSB-first: bit 0 of byte = leftmost pixel.
                        // Matches `pack_from_storage` depth=1 emit.
                        let row_off = sy as usize * row_bytes;
                        let byte = src_bytes[row_off + (sx as usize) / 8];
                        let bit = (byte >> (sx as usize & 7)) & 1;
                        u32::from(bit)
                    }
                    4 => {
                        let row_off = sy as usize * row_bytes;
                        let byte = src_bytes[row_off + (sx as usize) / 2];
                        u32::from(if (sx as usize).is_multiple_of(2) {
                            byte & 0x0f
                        } else {
                            (byte >> 4) & 0x0f
                        })
                    }
                    8 => {
                        let row_off = sy as usize * row_bytes;
                        u32::from(src_bytes[row_off + sx as usize])
                    }
                    24 | 32 => {
                        let off = sy as usize * row_bytes + sx as usize * 4;
                        u32::from_le_bytes([
                            src_bytes[off],
                            src_bytes[off + 1],
                            src_bytes[off + 2],
                            src_bytes[off + 3],
                        ])
                    }
                    _ => 0,
                };
                let mut rect = Vec::with_capacity(8);
                rect.extend_from_slice(&i16::to_le_bytes(dx));
                rect.extend_from_slice(&i16::to_le_bytes(dy));
                rect.extend_from_slice(&u16::to_le_bytes(1));
                rect.extend_from_slice(&u16::to_le_bytes(1));
                if pixel & plane != 0 {
                    fg_rects.extend_from_slice(&rect);
                } else {
                    bg_rects.extend_from_slice(&rect);
                }
            }
        }

        let foreground = self.core.current_foreground;
        let background = self.core.current_background;

        // Bg first, then fg — matches v1's overlap ordering so the
        // foreground wins on any aliased rect.
        if !bg_rects.is_empty() {
            self.poly_fill_rectangle(None, dst_host_xid, background, &bg_rects)?;
        }
        if !fg_rects.is_empty() {
            self.poly_fill_rectangle(None, dst_host_xid, foreground, &fg_rects)?;
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_put_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "put_image_unknown_xid");
            return Ok(());
        };
        // GC function + plane-mask (X11 §PutImage combines the wire
        // image with the destination through the full rop set):
        // non-Copy or partial plane-mask takes the CPU
        // read-modify-write path; NoOp is a spec no-op.
        {
            use yserver_core::backend::GcFunction;
            let function = self.core.current_function;
            if matches!(function, GcFunction::NoOp) {
                return Ok(());
            }
            let dst_depth = target.x11_depth();
            let full_mask = depth_plane_mask(dst_depth);
            let plane_mask = self.core.current_plane_mask & full_mask;
            if plane_mask == 0 {
                return Ok(());
            }
            let has_clip = !matches!(
                self.core.current_clip,
                yserver_core::backend::ClipState::None
            );
            let local = Rectangle16 {
                x: dst_x,
                y: dst_y,
                width,
                height,
            };
            // Storage shared with the window's children (a redirect
            // backing) holds their pixels too: ClipByChildren must leave
            // them, as the GC's composite clip does in Xorg. A window's
            // own leaf storage holds none, and keeps the fast path.
            let children_clip = (self.store.lookup(host_xid) != Some(target.backing_id()))
                .then(|| self.clip_fill_rects_by_subwindow_mode(host_xid, &[local]))
                .filter(|pieces| pieces.as_slice() != [local]);
            if has_clip || children_clip.is_some() {
                // A clipped upload must use per-run source offsets: the GPU
                // fast path accepts only a whole wire image and would paint
                // stale rows outside a rectangle clip. Bitmap clips likewise
                // lower to pixel runs here. Copy through apply_gc_function is
                // still exactly the source value.
                let pieces = children_clip.unwrap_or_else(|| vec![local]);
                let runs = self.intersect_with_current_clip_live(&pieces);
                for run in runs {
                    self.put_image_rop_cpu(
                        target.dst(),
                        ash::vk::Offset2D {
                            x: i32::from(run.x) + target.offset().0,
                            y: i32::from(run.y) + target.offset().1,
                        },
                        width,
                        (
                            i32::from(run.x) - i32::from(dst_x),
                            i32::from(run.y) - i32::from(dst_y),
                        ),
                        run.width,
                        run.height,
                        data,
                        depth,
                        function,
                        plane_mask,
                    );
                }
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::PutImage, target.backing_id(), 1);
                self.scene.wake_for_damage();
                return Ok(());
            }
            if !matches!(function, GcFunction::Copy) || plane_mask != full_mask {
                self.put_image_rop_cpu(
                    target.dst(),
                    ash::vk::Offset2D {
                        x: i32::from(dst_x) + target.offset().0,
                        y: i32::from(dst_y) + target.offset().1,
                    },
                    width,
                    (0, 0),
                    width,
                    height,
                    data,
                    depth,
                    function,
                    plane_mask,
                );
                self.telemetry.record_paint_submit();
                self.trace_simple(SubmitKind::PutImage, target.backing_id(), 1);
                self.scene.wake_for_damage();
                return Ok(());
            }
        }
        if let Err(e) = self.engine.put_image(
            &mut self.store,
            &mut self.platform,
            target.dst(),
            ash::vk::Offset2D {
                x: i32::from(dst_x) + target.offset().0,
                y: i32::from(dst_y) + target.offset().1,
            },
            ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
            data,
            depth,
        ) {
            log::warn!("render put_image: engine.put_image failed for xid {host_xid:#x}: {e:?}",);
        } else {
            self.telemetry.record_paint_submit();
            self.trace_simple(SubmitKind::PutImage, target.backing_id(), 1);
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_get_image(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        // Stage 4a — resolve through redirect routing per spec Risk 1
        // ("GetImage reads what the X server considers W's content,
        // which under redirect is B").
        //
        // THE INVARIANT (2026-09-11): the reply's depth and plane-mask
        // semantics come from the REQUESTED DRAWABLE, never from the
        // redirected backing or the scanout storage. Where the pixels
        // are read from and what depth the drawable is are two
        // different questions, and only the first one follows the
        // redirect routing.
        //
        // The comment this replaces assumed "backing is allocated to
        // match W's depth, so v1 / v2 see the same wire shape". Both
        // halves of that are false in the field:
        //
        //   root       the root DRAWABLE is depth 24
        //              (`resources::ROOT_DEPTH`), while its readback
        //              storage is 32-bit BGRA — we replied depth 32.
        //   routed     a depth-32 child of a redirected depth-24 frame
        //   child      paints into the FRAME's depth-24 backing, so the
        //              backing depth is 24 while the drawable is 32 —
        //              we replied depth 24.
        //
        // Measured against Xorg 21.1.24, which answers 24 and 32
        // respectively (`tools/depth32-bg-probe.c`, reply-depth line).
        //
        // This is a reply-header and plane-mask fix ONLY. The stored
        // CONTENT needs no reconstruction: the same probe, reading raw
        // image bytes rather than XGetPixel, shows our stored words are
        // already byte-identical to Xorg's for all six background cases
        // including the routed depth-32 child, whose alpha survives in
        // the depth-24 frame backing exactly as it does on Xorg.
        if host_xid == self.core.window_id {
            let Some(root_id) = self.store.lookup(self.core.window_id) else {
                self.log_render_gap("get_image_root_unknown_root");
                return Ok(None);
            };
            // The drawable is the ROOT WINDOW, whose X11 depth is a
            // protocol constant; `root_id`'s storage is the 32-bit
            // scanout readback buffer and says nothing about it.
            if self.store.get(root_id).is_none() {
                return Ok(None);
            }
            let depth = yserver_core::resources::ROOT_DEPTH;
            let mask = plane_mask & depth_plane_mask(depth);
            if format == GET_IMAGE_FORMAT_XY_PIXMAP && mask == 0 {
                return Ok(Some(wrap_get_image_reply(depth, Vec::new())));
            }
            // Root GetImage reads the composited scanout. The requested region
            // can span multiple outputs (multi-monitor root), so split it per
            // output and assemble; a single `read_scanout_region` rejects a
            // cross-output rect and yields an all-black reply. The rect is
            // already validated on-screen by the request handler, so it is not
            // re-clamped to root storage (which need not span the whole layout).
            let region = ash::vk::Rect2D {
                offset: ash::vk::Offset2D {
                    x: i32::from(x),
                    y: i32::from(y),
                },
                extent: ash::vk::Extent2D {
                    width: u32::from(width),
                    height: u32::from(height),
                },
            };
            let start = std::time::Instant::now();
            let readback = self.read_root_scanout_assembled(region);
            // Split at the readback boundary: everything above is the
            // pipeline drain + fence wait an asynchronous readback could
            // move off the loop thread; everything below is CPU packing
            // that has to happen either way. See `record_get_image_phases`.
            let readback_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let pack_start = std::time::Instant::now();
            let result = match readback {
                Some(mut pixel_bytes) => {
                    if format == GET_IMAGE_FORMAT_XY_PIXMAP {
                        pixel_bytes = z_to_xy_planes(
                            &pixel_bytes,
                            region.extent.width,
                            region.extent.height,
                            depth,
                            mask,
                        );
                    } else if mask != depth_plane_mask(depth) {
                        apply_z_plane_mask(&mut pixel_bytes, depth, mask);
                    }
                    let pack_ns =
                        u64::try_from(pack_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    let ns = readback_ns.saturating_add(pack_ns);
                    self.telemetry.record_one_shot_submit();
                    self.telemetry.record_fence_wait(ns);
                    self.telemetry.record_get_image_phases(readback_ns, pack_ns);
                    self.trace_simple(SubmitKind::GetImage, root_id, 1);
                    Ok(Some(wrap_get_image_reply(depth, pixel_bytes)))
                }
                None => Ok(None),
            };
            self.drain_frame_builder_telemetry();
            return result;
        }
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "get_image_unknown_xid");
            return Ok(None);
        };
        // `x11_depth()` is the depth of the drawable the CLIENT named;
        // `store.get(backing).depth` is the depth of whatever storage the
        // redirect routing landed on. The extent must come from the
        // storage (that is what is being read); the depth must not.
        let storage_extent = match self.store.get(target.backing_id()) {
            Some(d) => d.storage.extent,
            None => return Ok(None),
        };
        let depth = target.x11_depth();
        let mask = plane_mask & depth_plane_mask(depth);
        if format == GET_IMAGE_FORMAT_XY_PIXMAP && mask == 0 {
            // No planes requested: Xorg replies with zero data. This
            // path is load-bearing for Xlib — libX11's _XGetImage has
            // a NULL deref (`planes = image->depth` before the NULL
            // check) when an XYPixmap reply with plane_mask=0 carries
            // a non-zero length (xts5 Xlib9/XGetImage TP2 crashes,
            // poisons the display mutex, and hangs the whole TCM).
            return Ok(Some(wrap_get_image_reply(depth, Vec::new())));
        }
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: i32::from(x) + target.offset().0,
                y: i32::from(y) + target.offset().1,
            },
            extent: ash::vk::Extent2D {
                width: u32::from(width),
                height: u32::from(height),
            },
        };
        // Mirror the engine's clamp so the XY repack below knows the
        // row geometry of the bytes it gets back.
        //
        // #133 step 3 round 4: the bound is the STORAGE, not the content
        // rect. `GetImage` on a window is BORDER-INCLUSIVE in X11 — the
        // rectangle may reach `±bw` and the read is bounded by the
        // containing pixmap: Xorg `DoGetImage` checks exactly
        // `x >= -wBorderWidth(pWin) && x + width <= wBorderWidth(pWin) +
        // pDraw->width` (`dix/dispatch.c:2373-2377`), converts with
        // `relx = x + pDraw->x - pPix->screen_x` (`:2382-2390`) — this
        // target's content offset — and reads the BOUNDING drawable
        // (`:2405-2419`). Our own handler already allows `±bw`
        // (`process_request.rs:25566`, "xts XGetImage-7 reads (-1,-1)").
        //
        // So what keeps `x = 0` off the ring is the content OFFSET
        // applied above, never a clamp. Clamping to the content instead
        // returned fewer pixels than the client asked for, and libX11
        // sizes the XImage buffer from the reply length while indexing
        // it with the REQUESTED width and height — an out-of-bounds read
        // inside the client.
        let clipped = crate::kms::render::engine::clamp_rect(rect, storage_extent);
        let start = std::time::Instant::now();
        // SyncBoundary-flush attribution: this drawable-path readback does
        // 2 SyncBoundary flushes inside engine.get_image (gkrellm submit
        // storm, project_client_scheduling_fairness).
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::ClientGetImage);
        let readback = self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            target.src_including_border(),
            rect,
            depth,
        );
        // Split at the readback boundary — see the root path above.
        let readback_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let pack_start = std::time::Instant::now();
        let result = match readback {
            Ok(mut pixel_bytes) => {
                // INVARIANT: the reply must always describe the rectangle
                // the client asked for — Xorg computes the length as
                // `PixmapBytePad(width, depth) * height` up front
                // (`dix/dispatch.c:2227-2228`), before reading a pixel. A
                // short reply is an out-of-bounds read in the client, so
                // no bounds bug may ever be able to shorten one: pad here
                // rather than trusting every read path to stay in range.
                // Unreachable with the storage bound above plus the
                // handler's `±bw` check, hence the warning.
                let expected = wire_image_len(depth, u32::from(width), u32::from(height));
                if pixel_bytes.len() < expected {
                    log::warn!(
                        "render get_image: short read for xid {host_xid:#x} ({} of \
                         {expected} bytes for {width}x{height} d{depth}, requested \
                         {rect:?}, clipped {clipped:?}) — padding to the requested \
                         rectangle",
                        pixel_bytes.len(),
                    );
                    pixel_bytes.resize(expected, 0);
                }
                if matches!(depth, 24 | 32) && self.windows.contains_key(&host_xid) {
                    let area = ash::vk::Rect2D {
                        offset: ash::vk::Offset2D {
                            x: i32::from(x),
                            y: i32::from(y),
                        },
                        extent: ash::vk::Extent2D {
                            width: u32::from(width),
                            height: u32::from(height),
                        },
                    };
                    self.paste_inferiors(
                        host_xid,
                        target.backing_id(),
                        area,
                        depth,
                        &mut pixel_bytes,
                    );
                }
                if format == GET_IMAGE_FORMAT_XY_PIXMAP {
                    pixel_bytes = z_to_xy_planes(
                        &pixel_bytes,
                        u32::from(width),
                        u32::from(height),
                        depth,
                        mask,
                    );
                } else if mask != depth_plane_mask(depth) {
                    apply_z_plane_mask(&mut pixel_bytes, depth, mask);
                }
                let pack_ns = u64::try_from(pack_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                let ns = readback_ns.saturating_add(pack_ns);
                self.telemetry.record_one_shot_submit();
                self.telemetry.record_fence_wait(ns);
                self.telemetry.record_get_image_phases(readback_ns, pack_ns);
                self.trace_simple(SubmitKind::GetImage, target.backing_id(), 1);
                // X11 GetImage reply: 32-byte header + pixel rows.
                // The handler in `process_request.rs:handle_get_image`
                // patches `sequence` at [2..4] and `visual` at [8..12];
                // the rest of the header (depth, reply length in u32
                // units, padding) is the backend's job. Mirrors v1's
                // `KmsBackend::get_image` (kms/backend.rs:10400) — when
                // this returns just the pixel slice (no header), the
                // handler corrupts the first 32 bytes by writing into
                // them, and clients reading depth/length/sequence from
                // the wire see garbage.
                Ok(Some(wrap_get_image_reply(depth, pixel_bytes)))
            }
            Err(e) => {
                log::warn!(
                    "render get_image: engine.get_image failed for xid {host_xid:#x}: {e:?}",
                );
                Ok(None)
            }
        };
        // Phase B.1 Task 21: engine.get_image calls close_open_frame
        // (SyncWait reason) before blocking on the fence; drain the
        // resulting close event into telemetry.
        self.drain_frame_builder_telemetry();
        result
    }

    pub(in crate::kms::render::backend) fn backend_draw_read_depth1_pixmap(
        &mut self,
        _origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<Option<(u32, u32, Vec<u8>)>> {
        // SHAPE::Mask introspection — read a depth-1 mask pixmap
        // back as the tightly packed byte-per-pixel triple
        // `bitmap_to_yx_banded_rects` consumes. Mirrors v1's
        // `read_mirror_pixels` path (commit c5959af); without this
        // override the trait default returns `None` and every
        // ShapeMask degrades to a bounding-box rect — and since
        // the scene clips window draws to the bounding shape,
        // shaped popups render wrong (e16 hover clouds).
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "read_depth1_pixmap_unknown_xid");
            return Ok(None);
        };
        let (depth, extent, content_version) = match self.store.get(target.backing_id()) {
            Some(d) => (d.depth, d.storage.extent, d.content_version),
            None => return Ok(None),
        };
        if depth != 1 {
            return Ok(None);
        }
        // #32/#96: serve an unchanged depth-1 SHAPE::Mask from the CPU
        // cache instead of re-reading it from VRAM — the readback stalls
        // the single-threaded loop on discrete NVIDIA, and ~60% of these
        // reads re-fetch a mask that has not changed. `content_version`
        // is bumped on every draw into this pixmap, so a same-version hit
        // is guaranteed current; any draw forces a miss + fresh read.
        // `DrawableId` is never recycled, so a stale entry cannot alias a
        // reallocated pixmap.
        if let Some((w, h, bytes)) = self.depth1_mask_cache.get(
            target.backing_id(),
            content_version,
            extent.width,
            extent.height,
        ) {
            return Ok(Some((w, h, bytes)));
        }
        // #133 step 3 round 4: a whole-drawable read bounded by the
        // STORAGE, like `get_image` (SHAPE masks are pixmaps, so the
        // offset is `(0, 0)` in practice). The unpack loop below is
        // sized from `extent`, so the read must return exactly that
        // many rows — a content clamp here would hand it fewer.
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: target.offset().0,
                y: target.offset().1,
            },
            extent,
        };
        let start = std::time::Instant::now();
        self.telemetry
            .record_get_image_site(crate::kms::render::telemetry::GetImageSite::ReadDepth1);
        let result = match self.engine.get_image(
            &mut self.store,
            &mut self.platform,
            target.src_including_border(),
            rect,
            1,
        ) {
            Ok(packed) => {
                let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                self.telemetry.record_one_shot_submit();
                self.telemetry.record_fence_wait(ns);
                self.trace_simple(SubmitKind::GetImage, target.backing_id(), 1);
                // engine.get_image returns wire-format depth-1
                // rows (LSBFirst bits, 32-bit scanline pad);
                // unpack to one byte per pixel, 0xFF = set.
                let pack_start = std::time::Instant::now();
                let w = extent.width as usize;
                let row_bytes = extent.width.div_ceil(32) as usize * 4;
                let mut bytes = vec![0u8; w * extent.height as usize];
                for row in 0..extent.height as usize {
                    let src = &packed[row * row_bytes..];
                    for col in 0..w {
                        if src[col / 8] & (1 << (col % 8)) != 0 {
                            bytes[row * w + col] = 0xFF;
                        }
                    }
                }
                self.telemetry.record_get_image_phases(
                    ns,
                    u64::try_from(pack_start.elapsed().as_nanos()).unwrap_or(u64::MAX),
                );
                // #32/#96: cache this readback so the next unchanged
                // read of the same mask skips the VRAM round-trip.
                self.depth1_mask_cache.insert(
                    target.backing_id(),
                    content_version,
                    extent.width,
                    extent.height,
                    bytes.clone(),
                );
                Ok(Some((extent.width, extent.height, bytes)))
            }
            Err(e) => {
                log::warn!(
                    "render read_depth1_pixmap: engine.get_image failed for xid \
                         {host_xid:#x}: {e:?}",
                );
                Ok(None)
            }
        };
        // Same SyncWait close-event drain as get_image above.
        self.drain_frame_builder_telemetry();
        result
    }

    pub(in crate::kms::render::backend) fn backend_draw_poly_line(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "poly_line_unknown_xid");
            return Ok(());
        };
        // Cook the polyline vertices (coordinate_mode 0 = Origin
        // absolute, 1 = Previous deltas).
        let mut verts: Vec<(i32, i32)> = Vec::new();
        let mut prev: Option<(i32, i32)> = None;
        let mut offset = 0;
        while let Some((x, y)) = crate::kms::backend::read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if coordinate_mode == 1 {
                if let Some((px, py)) = prev {
                    (px + i32::from(x), py + i32::from(y))
                } else {
                    (i32::from(x), i32::from(y))
                }
            } else {
                (i32::from(x), i32::from(y))
            };
            verts.push((xi, yi));
            prev = Some((xi, yi));
        }
        let stroke = self.current_stroke_state(foreground);
        let out = crate::kms::render::stroke::stroke_path(
            &verts,
            crate::kms::render::stroke::StrokeShape::Polyline,
            &stroke,
        );
        self.emit_stroke_output(origin, host_xid, target, foreground, stroke.background, out);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_poly_segment(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "poly_segment_unknown_xid");
            return Ok(());
        };
        // Each segment is (x1:i16, y1:i16, x2:i16, y2:i16). Cook into
        // a flat (p0, p1, p0, p1, ...) vertex list for stroke_path's
        // DisjointSegments shape.
        let mut verts: Vec<(i32, i32)> = Vec::new();
        let mut offset = 0;
        while offset + 8 <= segments.len() {
            let Some((x1, y1)) = crate::kms::backend::read_i16_pair(segments, offset) else {
                break;
            };
            let Some((x2, y2)) = crate::kms::backend::read_i16_pair(segments, offset + 4) else {
                break;
            };
            offset += 8;
            verts.push((i32::from(x1), i32::from(y1)));
            verts.push((i32::from(x2), i32::from(y2)));
        }
        let stroke = self.current_stroke_state(foreground);
        let out = crate::kms::render::stroke::stroke_path(
            &verts,
            crate::kms::render::stroke::StrokeShape::DisjointSegments,
            &stroke,
        );
        self.emit_stroke_output(origin, host_xid, target, foreground, stroke.background, out);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_poly_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "poly_rectangle_unknown_xid");
            return Ok(());
        };
        let stroke = self.current_stroke_state(foreground);
        let mut fg_rects: Vec<Rectangle16> = Vec::new();
        let mut bg_rects: Vec<Rectangle16> = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = crate::kms::backend::read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            if r.width == 0 || r.height == 0 {
                continue;
            }
            // Per-rectangle polyline: 5 vertices, closes back to start
            // so the corner joins fire. fast-path width≤1 keeps this
            // bit-identical to the prior 4-edge-rect emission.
            let x0 = i32::from(r.x);
            let y0 = i32::from(r.y);
            let x1 = x0 + i32::from(r.width) - 1;
            let y1 = y0 + i32::from(r.height) - 1;
            let verts = [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)];
            let out = crate::kms::render::stroke::stroke_path(
                &verts,
                crate::kms::render::stroke::StrokeShape::Polyline,
                &stroke,
            );
            fg_rects.extend(out.fg_rects);
            bg_rects.extend(out.bg_rects);
        }
        self.emit_stroke_output(
            origin,
            host_xid,
            target,
            foreground,
            stroke.background,
            crate::kms::render::stroke::StrokeOutput { fg_rects, bg_rects },
        );
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_poly_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "poly_arc_unknown_xid");
            return Ok(());
        };
        // Each arc: x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16).
        // Walk each arc parametrically (honouring angle1/angle2 — partial
        // arcs no longer fall back to a full ellipse) into a chord
        // polyline, then run it through the stroke rasterizer so
        // line_width / cap_style / dashes apply. JoinStyle is irrelevant
        // within a single smooth arc.
        let stroke = self.current_stroke_state(foreground);
        let mut fg_rects: Vec<Rectangle16> = Vec::new();
        let mut bg_rects: Vec<Rectangle16> = Vec::new();
        for chunk in arcs.chunks_exact(12) {
            let ax = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]));
            let ay = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]));
            let aw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            let ah = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
            let angle1 = i16::from_le_bytes([chunk[8], chunk[9]]);
            let angle2 = i16::from_le_bytes([chunk[10], chunk[11]]);
            if aw <= 0 || ah <= 0 || angle2 == 0 {
                continue;
            }
            let cx = f64::from(ax) + f64::from(aw) * 0.5;
            let cy = f64::from(ay) + f64::from(ah) * 0.5;
            let rx = f64::from(aw) * 0.5;
            let ry = f64::from(ah) * 0.5;
            let verts = crate::kms::render::stroke::arc_polyline(cx, cy, rx, ry, angle1, angle2);
            let out = crate::kms::render::stroke::stroke_path(
                &verts,
                crate::kms::render::stroke::StrokeShape::Polyline,
                &stroke,
            );
            fg_rects.extend(out.fg_rects);
            bg_rects.extend(out.bg_rects);
        }
        self.emit_stroke_output(
            origin,
            host_xid,
            target,
            foreground,
            stroke.background,
            crate::kms::render::stroke::StrokeOutput { fg_rects, bg_rects },
        );
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_poly_point(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "poly_point_unknown_xid");
            return Ok(());
        };
        let mut rects = Vec::new();
        let mut prev = (0i32, 0i32);
        let mut first = true;
        let mut offset = 0;
        while let Some((x, y)) = crate::kms::backend::read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if coordinate_mode == 1 && !first {
                (prev.0 + i32::from(x), prev.1 + i32::from(y))
            } else {
                (i32::from(x), i32::from(y))
            };
            first = false;
            prev = (xi, yi);
            rects.push(Rectangle16 {
                x: xi.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                y: yi.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16,
                width: 1,
                height: 1,
            });
        }
        let background = self.core.current_background;
        self.emit_stroke_output(
            origin,
            host_xid,
            target,
            foreground,
            background,
            crate::kms::render::stroke::StrokeOutput {
                fg_rects: rects,
                bg_rects: Vec::new(),
            },
        );
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_poly_fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        // Each X11 Rectangle is 8 bytes: { i16 x, i16 y, u16 w, u16 h }.
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "poly_fill_rectangle_unknown_xid");
            return Ok(());
        };
        let mut rects = Vec::new();
        let mut offset = 0;
        while offset + 8 <= rectangles.len() {
            let Some(r) = crate::kms::backend::read_rect(rectangles, offset) else {
                break;
            };
            offset += 8;
            rects.push(r);
        }
        let rects = self.intersect_with_current_clip_live(&rects);
        self.fill_rects_honoring_fill_state(origin, host_xid, target, foreground, &rects);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_poly_fill_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "poly_fill_arc_unknown_xid");
            return Ok(());
        };
        // Each arc is 12 bytes: x(i16) y(i16) w(u16) h(u16) angle1(i16) angle2(i16).
        // Build the closed fill polygon per the GC's ArcMode (Chord vs
        // PieSlice), honouring angle1/angle2 (partial arcs no longer
        // fill the full ellipse), then scanline-fill it.
        let arc_mode = self.core.current_arc_mode;
        let (img_w, img_h) = self
            .drawable_dims(host_xid)
            .map(|(w, h)| (w as i32, h as i32))
            .unwrap_or((0, 0));
        let mut rects: Vec<Rectangle16> = Vec::new();
        for chunk in arcs.chunks_exact(12) {
            let ax = i32::from(i16::from_le_bytes([chunk[0], chunk[1]]));
            let ay = i32::from(i16::from_le_bytes([chunk[2], chunk[3]]));
            let aw = i32::from(u16::from_le_bytes([chunk[4], chunk[5]]));
            let ah = i32::from(u16::from_le_bytes([chunk[6], chunk[7]]));
            let angle1 = i16::from_le_bytes([chunk[8], chunk[9]]);
            let angle2 = i16::from_le_bytes([chunk[10], chunk[11]]);
            if aw <= 0 || ah <= 0 || angle2 == 0 {
                continue;
            }
            let cx = f64::from(ax) + f64::from(aw) * 0.5;
            let cy = f64::from(ay) + f64::from(ah) * 0.5;
            let rx = f64::from(aw) * 0.5;
            let ry = f64::from(ah) * 0.5;
            let verts = crate::kms::render::stroke::fill_arc_polygon(
                cx, cy, rx, ry, angle1, angle2, arc_mode,
            );
            crate::kms::backend::scanline_fill_polygon(&verts, &mut rects);
        }
        if !rects.is_empty() {
            let clipped = crate::kms::backend::clip_rects_to_image(&rects, img_w, img_h);
            let rects = self.intersect_with_current_clip_live(&clipped);
            self.fill_rects_honoring_fill_state(origin, host_xid, target, foreground, &rects);
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_fill_poly(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "fill_poly_unknown_xid");
            return Ok(());
        };
        // i16 vertex pairs. coord_mode 0 = Origin (absolute), 1 = Previous.
        let mut verts: Vec<(i32, i32)> = Vec::with_capacity(points.len() / 4);
        let mut offset = 0;
        let mut last = (0i32, 0i32);
        while let Some((x, y)) = crate::kms::backend::read_i16_pair(points, offset) {
            offset += 4;
            let (xi, yi) = if coord_mode == 1 && !verts.is_empty() {
                (last.0 + i32::from(x), last.1 + i32::from(y))
            } else {
                (i32::from(x), i32::from(y))
            };
            verts.push((xi, yi));
            last = (xi, yi);
        }
        let mut rects: Vec<Rectangle16> = Vec::new();
        crate::kms::backend::scanline_fill_polygon(&verts, &mut rects);
        let (img_w, img_h) = self
            .drawable_dims(host_xid)
            .map(|(w, h)| (w as i32, h as i32))
            .unwrap_or((0, 0));
        let clipped = crate::kms::backend::clip_rects_to_image(&rects, img_w, img_h);
        let rects = self.intersect_with_current_clip_live(&clipped);
        self.fill_rects_honoring_fill_state(origin, host_xid, target, foreground, &rects);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let Some(target) = self.resolve_paint_target(host_xid) else {
            self.log_unresolved_target(host_xid, "fill_rectangle_unknown_xid");
            return Ok(());
        };
        let rects = self.intersect_with_current_clip_live(&[Rectangle16 {
            x,
            y,
            width,
            height,
        }]);
        self.fill_rects_honoring_fill_state(origin, host_xid, target, foreground, &rects);
        Ok(())
    }
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_draw_poly_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM8.
        // Each TEXTITEM8 is `len(u8) delta(i8) chars(len)` for len
        // in 0..=254, or `255 font_id(u32 BE)` for a font change.
        // No inter-item padding.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut items = &body[12..];
        let mut cursor_x = x;
        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.core.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            if items.len() < 2 + len {
                break;
            }
            let text = &items[2..2 + len];
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            if !text.is_empty() {
                let chars: Vec<char> = text.iter().map(|&b| b as char).collect();
                self.render_text_chars(origin, host_xid, foreground, cursor_x, y, &chars)?;
                if let Some(font_state) =
                    self.core.current_font.and_then(|f| self.core.fonts.get(&f))
                {
                    cursor_x = cursor_x.saturating_add(text_advance(font_state, &chars));
                }
            }
            items = &items[2 + len..];
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_poly_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + LISTofTEXTITEM16.
        // Each TEXTITEM16 is `len(u8) delta(i8) chars(2*len)` (chars
        // are CHAR2B, big-endian) for len in 0..=254, or `255
        // font_id(u32 BE)` for a font change.
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut cursor_x = x;
        let mut items = &body[12..];
        while items.len() >= 2 {
            let len = items[0];
            if len == 255 {
                if items.len() < 5 {
                    break;
                }
                let font_xid = u32::from_be_bytes([items[1], items[2], items[3], items[4]]);
                self.core.current_font = Some(font_xid);
                items = &items[5..];
                continue;
            }
            let delta = items[1] as i8;
            let len = len as usize;
            let needed = 2 + 2 * len;
            if items.len() < needed {
                break;
            }
            cursor_x = cursor_x.saturating_add(i32::from(delta));
            let mut chars = Vec::with_capacity(len);
            for i in 0..len {
                let codepoint = u16::from_be_bytes([items[2 + 2 * i], items[2 + 2 * i + 1]]) as u32;
                chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
            }
            if !chars.is_empty() {
                self.render_text_chars(origin, host_xid, foreground, cursor_x, y, &chars)?;
                if let Some(font_state) =
                    self.core.current_font.and_then(|f| self.core.fonts.get(&f))
                {
                    cursor_x = cursor_x.saturating_add(text_advance(font_state, &chars));
                }
            }
            items = &items[needed..];
        }
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_draw_image_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        // Body: drawable(4) + gc(4) + x(2) + y(2) + string(text_len)
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let end = (12usize + text_len as usize).min(body.len());
        let chars: Vec<char> = body[12..end].iter().map(|&b| b as char).collect();
        self.image_text_common(origin, host_xid, foreground, background, x, y, &chars)
    }

    pub(in crate::kms::render::backend) fn backend_draw_image_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        let x = i16::from_le_bytes([body[8], body[9]]) as i32;
        let y = i16::from_le_bytes([body[10], body[11]]) as i32;
        let mut chars = Vec::with_capacity(text_len as usize);
        let mut pos = 12usize;
        for _ in 0..text_len {
            if pos + 2 > body.len() {
                break;
            }
            let codepoint = u16::from_be_bytes([body[pos], body[pos + 1]]) as u32;
            pos += 2;
            chars.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
        }
        self.image_text_common(origin, host_xid, foreground, background, x, y, &chars)
    }
}
