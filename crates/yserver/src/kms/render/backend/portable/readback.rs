//! Root reads: the scene composes the root for a read, and a root region is
//! assembled from per-output pieces.

use crate::kms::render::{
    backend::{
        RootReadSource, RootScanoutRead, XBackend,
        portable::{compute_copy_area_dst_rects, depth_plane_mask},
        seam::Platform,
    },
    submit_trace::SubmitKind,
    target::PaintTarget,
};
use ash::vk;
use std::io;
use yserver_core::backend::Backend as _;

impl<P: Platform> XBackend<P> {
    /// Screenshot fast-path for `CopyArea(src=root, …)` with the GC's
    /// subwindow-mode set to `IncludeInferiors` (Qt5 `QScreen::grabWindow`,
    /// e.g. flameshot). The root window's own storage holds only the
    /// background, so the ordinary copy would capture just the wallpaper. Read
    /// the COMPOSITED scanout instead — the same source `get_image` uses for
    /// the root — and upload it into the destination. This keeps composition in
    /// one place (the scene compositor) rather than re-walking the window tree.
    ///
    /// Returns `Ok(true)` when it handled the copy, `Ok(false)` to fall through
    /// to the normal path: for non-root sources, `ClipByChildren` (root storage
    /// is the correct background-only source there), non-plain GC state
    /// (non-`GXcopy`, partial plane-mask, or a bitmap clip-mask — whose
    /// semantics a raw upload can't preserve), or when there are no live
    /// outputs to read.
    pub(in crate::kms::render::backend) fn try_copy_area_root_scanout(
        &mut self,
        src_host_xid: u32,
        dst_host_xid: u32,
        dst_target: &PaintTarget,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<bool> {
        use yserver_core::backend::{ClipState, GcFunction, SubwindowMode};
        if src_host_xid != self.core.window_id
            || !matches!(
                self.core.current_subwindow_mode,
                SubwindowMode::IncludeInferiors
            )
        {
            return Ok(false);
        }
        // Only the plain GXcopy / full-plane-mask / no-bitmap-clip case reads
        // the scanout; exotic ROP/plane/clip-mask combos fall through so their
        // semantics aren't silently reduced to a raw upload. (GC rectangle
        // clips ARE honoured — via `compute_copy_area_scissors` below.)
        let dst_depth = dst_target.x11_depth();
        let full_mask = depth_plane_mask(dst_depth);
        let plane_mask = self.core.current_plane_mask & full_mask;
        let plain = matches!(self.core.current_function, GcFunction::Copy)
            && plane_mask == full_mask
            && !matches!(self.core.current_clip, ClipState::Pixmap { .. });
        if !plain {
            return Ok(false);
        }
        self.prime_transformed_root_reads();
        // Root space: a transformed output covers its footprint (spec D6).
        let outputs = self.crtc_root_rects();
        if outputs.is_empty() {
            return Ok(false);
        }
        // Destination sub-rects honour the GC rectangle clip (and, for window
        // destinations, child/occluder subtraction). Pixmap destinations get
        // the whole rect. Empty = fully clipped away (spec-correct no-op).
        let sub_rects =
            self.compute_copy_area_scissors(dst_host_xid, dst_target, dst_x, dst_y, width, height);
        if sub_rects.is_empty() {
            return Ok(true);
        }
        let dst_id = dst_target.dst();
        let (off_x, off_y) = dst_target.offset();
        let mut any = false;
        for sub in &sub_rects {
            for piece in split_root_scanout_reads(*sub, src_x, src_y, dst_x, dst_y, &outputs) {
                let bytes = match P::read_on_screen_region(self, piece.read) {
                    Ok(b) => b,
                    Err(e) => {
                        log::warn!(
                            "render copy_area root-scanout: readback failed \
                             (dst=0x{dst_host_xid:x} read={:?}): {e:?}",
                            piece.read,
                        );
                        continue;
                    }
                };
                let dst_pos = ash::vk::Offset2D {
                    x: piece.dst_local.x + off_x,
                    y: piece.dst_local.y + off_y,
                };
                match self.engine.put_image(
                    &mut self.store,
                    self.platform.gpu_mut(),
                    dst_id,
                    dst_pos,
                    piece.read.extent,
                    &bytes,
                    dst_depth,
                ) {
                    Ok(()) => any = true,
                    Err(e) => log::warn!(
                        "render copy_area root-scanout: upload failed \
                         (dst=0x{dst_host_xid:x} pos={dst_pos:?}): {e:?}"
                    ),
                }
            }
        }
        if any {
            self.telemetry.record_paint_submit();
            self.trace_simple(SubmitKind::CopyArea, dst_target.backing_id(), 1);
            self.scene.wake_for_damage();
        }
        Ok(true)
    }

    /// The image a root read of `local` on output `output_idx` copies from,
    /// composed from the scene as every earlier request left it
    /// ([`SceneCompositor::root_readback`]); `None` without a live scene,
    /// leaving the read on the scanout.
    pub(crate) fn fresh_root_readback(
        &mut self,
        output_idx: usize,
        local: vk::Rect2D,
    ) -> Option<vk::Image> {
        if !self.scene.is_live() {
            return None;
        }
        if !self.scene.root_readback_is_current(
            &self.store,
            &self.platform.output_layouts(),
            output_idx,
            local,
        ) {
            if let Err(e) = self.engine.close_open_frame(
                &mut self.store,
                self.platform.gpu_mut(),
                crate::kms::render::frame_builder::CloseReason::LegacyRootRead,
            ) {
                log::warn!("render root read: close_open_frame failed: {e:?}");
            }
            if let Err(e) = self.engine.flush_submit_group(
                &mut self.store,
                self.platform.gpu_mut(),
                crate::kms::render::submit_group::FlushReason::SceneCompose,
            ) {
                log::warn!("render root read: flush_submit_group failed: {e:?}");
            }
        }
        let cow_host_xid = self.cow_host_xid();
        let outputs = self.platform.output_layouts();
        let input = crate::kms::render::backend::scene_input(
            &self.core,
            &self.windows,
            &outputs,
            self.platform.fb_size(),
        );
        match self.scene.root_readback(
            &input,
            &mut self.store,
            &self.platform,
            cow_host_xid,
            output_idx,
            local,
        ) {
            Ok(image) => image,
            Err(e) => {
                log::warn!("render root read: output {output_idx} readback compose failed: {e}");
                None
            }
        }
    }

    /// Before a root read: compose any transformed output that has not
    /// composed since its transform became current, so the read returns root
    /// content rather than a zero-filled piece.
    fn prime_transformed_root_reads(&mut self) {
        if !self
            .scene
            .has_unprimed_transform_intermediate(&self.platform.output_layouts())
        {
            return;
        }
        if let Err(e) = self.engine.close_open_frame(
            &mut self.store,
            self.platform.gpu_mut(),
            crate::kms::render::frame_builder::CloseReason::LegacyRootRead,
        ) {
            log::warn!("render root read: close_open_frame failed: {e:?}");
        }
        if let Err(e) = self.engine.flush_submit_group(
            &mut self.store,
            self.platform.gpu_mut(),
            crate::kms::render::submit_group::FlushReason::SceneCompose,
        ) {
            log::warn!("render root read: flush_submit_group failed: {e:?}");
        }
        let cow_host_xid = self.cow_host_xid();
        let outputs = self.platform.output_layouts();
        let input = crate::kms::render::backend::scene_input(
            &self.core,
            &self.windows,
            &outputs,
            self.platform.fb_size(),
        );
        if let Err(e) = self.scene.prime_transform_intermediates(
            &input,
            &mut self.store,
            &self.platform,
            cow_host_xid,
        ) {
            log::warn!("render root read: transform intermediate compose failed: {e}");
        }
    }

    pub(in crate::kms::render::backend) fn read_root_scanout_assembled(
        &mut self,
        region: vk::Rect2D,
    ) -> Option<Vec<u8>> {
        self.prime_transformed_root_reads();
        // Root space: a transformed output covers its footprint (spec D6).
        let outputs = self.crtc_root_rects();
        let root_id = self.store.lookup(self.core.window_id)?;
        Some(assemble_root_scanout(region, &outputs, |rect, source| {
            if source == RootReadSource::Background {
                // No CRTC shows this area, so no scanout holds it. The root
                // storage does: its background, which is what Xorg's screen
                // pixmap holds there too (windows over it are not composed
                // outside the CRTCs, so they are missing from this piece).
                return self
                    .engine
                    .get_image(
                        &mut self.store,
                        self.platform.gpu_mut(),
                        crate::kms::render::target::Src::server_internal(root_id),
                        rect,
                        32,
                    )
                    .map_err(|e| log::debug!("render root background readback {rect:?}: {e:?}"))
                    .ok()
                    .filter(|bytes| {
                        bytes.len() == rect.extent.width as usize * rect.extent.height as usize * 4
                    });
            }
            // `assemble_root_scanout` zero-fills a piece it cannot read. That
            // degradation is unchanged, but a failure here now also covers an
            // unresolvable direct-scanout source, which previously answered
            // with a stale composed BO instead — so say which rect went black.
            match P::read_on_screen_region(self, rect) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    if let Some(held) = self.root_readback_warn.check(std::time::Instant::now()) {
                        log::warn!(
                            "render root scanout readback: {rect:?} unreadable, \
                             zero-filling that piece: {error} ({held} more since the last report)"
                        );
                    }
                    None
                }
            }
        }))
    }
}

/// Split one surviving destination sub-rect (destination-LOCAL coords) of a
/// root-source `CopyArea` into per-output scanout reads.
///
/// `read_scanout_region(OnScreenOnly)` rejects any rect that is partially
/// off-screen or spans two outputs (`select_scanout_read_route` requires the
/// rect to sit fully inside one output BO). So for each output we intersect the
/// requested root-absolute source region with the output's bounds and emit one
/// read per non-empty piece. Source area not covered by any output is dropped
/// (X11 leaves off-screen root pixels undefined).
///
/// `outputs` entries are `(x, y, width, height)` in root-absolute coordinates.
pub(in crate::kms::render::backend) fn split_root_scanout_reads(
    dst_local: vk::Rect2D,
    src_x: i16,
    src_y: i16,
    dst_x: i16,
    dst_y: i16,
    outputs: &[(i32, i32, u32, u32)],
) -> Vec<RootScanoutRead> {
    // Root-absolute source origin for this destination sub-rect: shift the
    // wire src origin by the same delta the sub-rect shifted from the wire dst
    // origin (mirrors copy_area's `src = src_xy + (sub - dst_xy)` arithmetic).
    let src_ox = i32::from(src_x) + (dst_local.offset.x - i32::from(dst_x));
    let src_oy = i32::from(src_y) + (dst_local.offset.y - i32::from(dst_y));
    let src_x1 = src_ox.saturating_add_unsigned(dst_local.extent.width);
    let src_y1 = src_oy.saturating_add_unsigned(dst_local.extent.height);
    let mut reads = Vec::new();
    for &(ox, oy, ow, oh) in outputs {
        let ix0 = src_ox.max(ox);
        let iy0 = src_oy.max(oy);
        let ix1 = src_x1.min(ox.saturating_add_unsigned(ow));
        let iy1 = src_y1.min(oy.saturating_add_unsigned(oh));
        if ix1 <= ix0 || iy1 <= iy0 {
            continue;
        }
        reads.push(RootScanoutRead {
            read: vk::Rect2D {
                offset: vk::Offset2D { x: ix0, y: iy0 },
                extent: vk::Extent2D {
                    width: (ix1 - ix0).unsigned_abs(),
                    height: (iy1 - iy0).unsigned_abs(),
                },
            },
            // Destination-local: the piece's offset within the requested
            // source region, added back onto the sub-rect's dst origin.
            dst_local: vk::Offset2D {
                x: dst_local.offset.x + (ix0 - src_ox),
                y: dst_local.offset.y + (iy0 - src_oy),
            },
        });
    }
    reads
}

/// Assemble a root-region `GetImage` ZPixmap buffer from per-output scanout
/// reads.
///
/// The plain root `GetImage` path — unlike `CopyArea`-from-root, which already
/// splits — used to issue ONE `read_scanout_region` for the whole rect.
/// `select_scanout_read_route` rejects any rect that isn't fully inside a
/// single output, so a multi-monitor full-root grab matched no BO and the reply
/// came back all-black (ImageMagick `import` screenshots over a dual-head root:
/// `import` grabs the entire root, then crops client-side). Split the region
/// per output, read each piece, and blit it into one row-major 4-bytes-per-pixel
/// buffer. Region area not covered by any output is read as
/// `RootReadSource::Background`: Xorg's root is the whole screen pixmap, so
/// e.g. the corner a 1280x800 + 1024x768 layout leaves uncovered answers the
/// root background, not black.
///
/// `read(rect, source)` returns tightly-packed 4-bpp rows for `rect` (for
/// `Scanout`, guaranteed by the splitter to sit fully within one output) or
/// `None` if that read failed — a failed piece is left zero-filled rather than
/// aborting the whole capture.
pub(in crate::kms::render::backend) fn assemble_root_scanout<F>(
    region: vk::Rect2D,
    outputs: &[(i32, i32, u32, u32)],
    mut read: F,
) -> Vec<u8>
where
    F: FnMut(vk::Rect2D, RootReadSource) -> Option<Vec<u8>>,
{
    let w = region.extent.width as usize;
    let h = region.extent.height as usize;
    let stride = w * 4;
    let mut assembled = vec![0u8; stride * h];
    // src origin = region origin, dst origin = 0 → each piece's `dst_local` is
    // its offset within the assembled buffer.
    let src_x = i16::try_from(region.offset.x).unwrap_or(i16::MAX);
    let src_y = i16::try_from(region.offset.y).unwrap_or(i16::MAX);
    let sub = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: region.extent,
    };
    let output_rects: Vec<vk::Rect2D> = outputs
        .iter()
        .map(|&(x, y, width, height)| vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D { width, height },
        })
        .collect();
    let scanout = split_root_scanout_reads(sub, src_x, src_y, 0, 0, outputs)
        .into_iter()
        .map(|piece| (piece, RootReadSource::Scanout));
    let background = compute_copy_area_dst_rects(region, &output_rects)
        .into_iter()
        .map(|rect| {
            let piece = RootScanoutRead {
                read: rect,
                dst_local: vk::Offset2D {
                    x: rect.offset.x - region.offset.x,
                    y: rect.offset.y - region.offset.y,
                },
            };
            (piece, RootReadSource::Background)
        });
    for (piece, source) in scanout.chain(background) {
        let Some(bytes) = read(piece.read, source) else {
            continue;
        };
        let pw = piece.read.extent.width as usize;
        let ph = piece.read.extent.height as usize;
        let dx = usize::try_from(piece.dst_local.x).unwrap_or(0);
        let dy = usize::try_from(piece.dst_local.y).unwrap_or(0);
        let row_bytes = pw * 4;
        for row in 0..ph {
            let src = row * row_bytes;
            let dst = (dy + row) * stride + dx * 4;
            let (Some(src_end), Some(dst_end)) =
                (src.checked_add(row_bytes), dst.checked_add(row_bytes))
            else {
                break;
            };
            if src_end <= bytes.len() && dst_end <= assembled.len() {
                assembled[dst..dst_end].copy_from_slice(&bytes[src..src_end]);
            }
        }
    }
    assembled
}
