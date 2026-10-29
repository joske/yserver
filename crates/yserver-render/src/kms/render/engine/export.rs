use crate::kms::render::{
    engine::{RenderEngine, RenderError},
    gpu::GpuCore,
    store::{DrawableId, DrawableStore},
};
use ash::vk;
use std::sync::Arc;

/// A scanout candidate's exported layout, handed to the probe of
/// [`RenderEngine::promote_drawable_for_scanout`] before any storage swap.
#[derive(Debug)]
pub struct ScanoutCandidate<'a> {
    pub fd: std::os::fd::BorrowedFd<'a>,
    pub width: u32,
    pub height: u32,
    pub format: vk::Format,
    pub depth: u8,
    pub offset: u64,
    pub pitch: u32,
    pub modifier: u64,
}

/// An adopted scanout promotion: the candidate's export (which owns its
/// dma-buf fd) and what the probe returned.
#[derive(Debug)]
pub struct ScanoutPromotion<T> {
    pub export: crate::kms::vk::dri3::DmabufExport,
    pub accepted: T,
}

/// Why [`RenderEngine::promote_drawable_for_scanout`] kept the old storage.
#[derive(Debug)]
pub enum ScanoutPromoteError<E> {
    /// The drawable is already exportable; nothing was allocated.
    AlreadyExportable,
    Allocate(vk::Result),
    Export(vk::Result),
    Probe(E),
    Render(RenderError),
}

impl RenderEngine {
    /// GLX-TFP (Task 1.2): permanently migrate a server-owned pixmap
    /// onto dma-buf-exportable storage (glamor's model). Idempotent —
    /// returns early if the drawable is already exportable (DRI3 import
    /// or a prior promotion).
    ///
    /// Allocates an exportable image, then swaps it in through
    /// [`Self::adopt_candidate`].
    ///
    /// # Errors
    ///
    /// `NoVk` (stub engine), `UnknownDrawable`, or any propagated
    /// `vk::Result` from allocation / view creation / the blocking copy.
    pub fn promote_drawable_exportable(
        &mut self,
        gpu: &mut GpuCore,
        store: &mut DrawableStore,
        id: DrawableId,
    ) -> Result<(), RenderError> {
        if self.inner.is_none() {
            return Err(RenderError::NoVk);
        }
        // Idempotency check first — avoid the flush cost if already done.
        let (extent, format) = {
            let d = store.get(id).ok_or(RenderError::UnknownDrawable(id))?;
            if d.storage.is_exportable() {
                return Ok(());
            }
            (d.storage.extent, d.storage.format)
        };
        let vk = gpu.vk().ok_or(RenderError::NoVk)?.clone();

        // (a) allocate exportable target.
        let exp =
            crate::kms::vk::target::allocate_exportable(&vk, extent.width, extent.height, format)?;
        self.adopt_candidate(gpu, store, id, exp, None)
            .map_err(|(error, _candidate)| error)
    }

    /// Promote a server-owned pixmap onto linear scanout storage, adopting
    /// the candidate only after `probe` accepts its exported layout.
    ///
    /// Allocates with [`crate::kms::vk::target::allocate_scanout_pixmap`]
    /// (never touching the GLX TFP tiling strategy), exports the candidate,
    /// runs `probe` on that dma-buf, and only on `Ok` swaps it in through
    /// [`Self::adopt_candidate`]. Any failure before the swap leaves the
    /// drawable untouched: no frame close, no flush, no storage change.
    /// On a failed adopt the probe result drops first, then the fd, then the
    /// candidate image.
    ///
    /// # Errors
    ///
    /// [`ScanoutPromoteError`]; the drawable keeps its storage in every case.
    pub fn promote_drawable_for_scanout<T, E>(
        &mut self,
        gpu: &mut GpuCore,
        store: &mut DrawableStore,
        id: DrawableId,
        plane_modifiers: &[&[u64]],
        probe: impl FnOnce(&ScanoutCandidate<'_>) -> Result<T, E>,
    ) -> Result<ScanoutPromotion<T>, ScanoutPromoteError<E>> {
        if self.inner.is_none() {
            return Err(ScanoutPromoteError::Render(RenderError::NoVk));
        }
        let (extent, format, depth) =
            {
                let d = store.get(id).ok_or(ScanoutPromoteError::Render(
                    RenderError::UnknownDrawable(id),
                ))?;
                if d.storage.is_exportable() {
                    return Err(ScanoutPromoteError::AlreadyExportable);
                }
                (d.storage.extent, d.storage.format, d.storage.depth)
            };
        let vk = gpu
            .vk()
            .ok_or(ScanoutPromoteError::Render(RenderError::NoVk))?
            .clone();
        let candidate = crate::kms::vk::target::allocate_scanout_pixmap(
            &vk,
            extent.width,
            extent.height,
            format,
            plane_modifiers,
        )
        .map_err(ScanoutPromoteError::Allocate)?;
        let export = crate::kms::vk::dri3::export_backing(&vk, &candidate)
            .map_err(ScanoutPromoteError::Export)?;
        let accepted = {
            use std::os::fd::AsFd as _;
            probe(&ScanoutCandidate {
                fd: export.fd.as_fd(),
                width: extent.width,
                height: extent.height,
                format,
                depth,
                offset: candidate.offset,
                pitch: export.stride,
                modifier: candidate.modifier,
            })
            .map_err(ScanoutPromoteError::Probe)?
        };
        // Scanout storage is not an external export until a client takes it.
        let category = Some(crate::kms::vk::mem_accounting::MemCategory::Scanout);
        match self.adopt_candidate(gpu, store, id, candidate, category) {
            Ok(()) => Ok(ScanoutPromotion { export, accepted }),
            Err((error, candidate)) => {
                drop(accepted);
                drop(export);
                drop(candidate);
                Err(ScanoutPromoteError::Render(error))
            }
        }
    }

    /// Swap a freshly allocated exportable image in as `id`'s storage.
    ///
    /// Steps: close the open frame and flush the submit group; copy the old
    /// content into the candidate and block until the copy completes; build
    /// fresh sample + attachment views over it; swap the `Storage` handles;
    /// invalidate the drawable's cached views (the cache keys on
    /// `DrawableId` and never re-checks the `VkImage`, so a swap without
    /// this keeps sampling the old image); retire the old handles once
    /// their guarding fence signals.
    ///
    /// On error the candidate is handed back unconsumed and the storage is
    /// unchanged. `category` accounts the new memory; `None` takes the
    /// export category of the storage it replaces.
    fn adopt_candidate(
        &mut self,
        gpu: &mut GpuCore,
        store: &mut DrawableStore,
        id: DrawableId,
        exp: crate::kms::vk::target::ExportableImage,
        category: Option<crate::kms::vk::mem_accounting::MemCategory>,
    ) -> Result<(), (RenderError, crate::kms::vk::target::ExportableImage)> {
        // Submit-boundary (codex review): any open frame / parked submit
        // group may hold CBs that captured cached views of the OLD image
        // but have NOT been submitted to the queue yet. The copy below
        // relies on same-queue submission ordering to read finalized
        // old-image content, and `retire_image_after` later destroys the
        // old handles + invalidated views once `last_render_ticket`
        // signals. Both are only sound if every prior user of the old
        // image is already submitted. Close the open frame and flush the
        // submit group here to establish that boundary (mirrors
        // `get_image`'s SyncWait close before its readback). After this,
        // `last_render_ticket` names the newest in-flight ticket that
        // touched the drawable.
        if let Err(e) = self.close_open_frame(
            store,
            gpu,
            crate::kms::render::frame_builder::CloseReason::SyncWait,
        ) {
            return Err((e, exp));
        }
        if let Err(e) = self.flush_submit_group(
            store,
            gpu,
            crate::kms::render::submit_group::FlushReason::SyncBoundary,
        ) {
            return Err((e.into(), exp));
        }

        // Metadata read (post-close: current_layout reflects any
        // transition the frame close recorded).
        let Some((extent, format, depth, old_layout, old_image)) = store.get(id).map(|d| {
            let s = &d.storage;
            (s.extent, s.format, s.depth, s.current_layout, s.image)
        }) else {
            return Err((RenderError::UnknownDrawable(id), exp));
        };
        let Some(vk) = gpu.vk().cloned() else {
            return Err((RenderError::NoVk, exp));
        };

        // Copy old content → new image, block until complete.
        if let Err(e) = self.copy_image_blocking(gpu, old_image, old_layout, exp.image, extent) {
            return Err((e, exp));
        }
        let new_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;

        // Build new views (same depth-aware swizzle the store uses).
        let sample_view = match GpuCore::build_sample_view(&vk, exp.image, format, depth) {
            Ok(v) => v,
            Err(e) => return Err((e.into(), exp)),
        };
        let image_view = match GpuCore::build_attachment_view(&vk, exp.image, format) {
            Ok(v) => v,
            Err(e) => {
                unsafe { vk.device.destroy_image_view(sample_view, None) };
                return Err((e.into(), exp));
            }
        };

        // Swap storage, taking ownership of the exportable image's raw
        // handles. The drawable existence check happens BEFORE
        // `into_raw_parts` so an absent drawable can't leak the four raw
        // handles (`into_raw_parts` disarms `ExportableImage`'s Drop).
        let Some(d) = store.get_mut(id) else {
            unsafe {
                vk.device.destroy_image_view(image_view, None);
                vk.device.destroy_image_view(sample_view, None);
            }
            return Err((RenderError::UnknownDrawable(id), exp));
        };
        let (exp_image, exp_memory, exp_stride, exp_size, exp_offset, exp_modifier) =
            exp.into_raw_parts();
        // A promoted redirect backing stays distinguishable from other exports.
        crate::kms::vk::mem_accounting::recategorise(
            exp_memory,
            category.unwrap_or_else(|| {
                crate::kms::vk::mem_accounting::export_category_for(
                    crate::kms::vk::mem_accounting::category_of(d.storage.memory),
                )
            }),
        );
        let retired = d.storage.adopt_exportable(
            exp_image,
            exp_memory,
            sample_view,
            image_view,
            new_layout,
            exp_stride,
            exp_size,
            exp_offset,
            exp_modifier,
        );

        // Invalidate the view cache for this DrawableId.
        self.invalidate_drawable_views(id);

        // Retire old handles once the old image's last render fence
        // signals (clone the ticket; None → retire eagerly).
        let guard = store.get(id).and_then(|d| d.last_render_ticket.clone());
        self.retire_image_after(retired, guard);
        Ok(())
    }

    /// GLX-TFP (Task 1.2): blocking old→new image copy used by the
    /// promotion path. Records, on a dedicated one-shot CB + fence
    /// (`run_one_shot_op` waits for the fence before returning):
    ///   - `src`: `src_layout` → `TRANSFER_SRC_OPTIMAL`
    ///   - `dst`: `UNDEFINED` → `TRANSFER_DST_OPTIMAL`
    ///   - `vkCmdCopyImage` (full `extent`, COLOR, 1 mip / 1 layer)
    ///   - `dst`: `TRANSFER_DST_OPTIMAL` → `SHADER_READ_ONLY_OPTIMAL`
    ///
    /// Promotion is rare (once per pixmap, on first GLX bind), so a
    /// dedicated fence wait is acceptable.
    ///
    /// # Errors
    ///
    /// `NoVk`, or any propagated `vk::Result` from CB recording / submit
    /// / fence wait.
    fn copy_image_blocking(
        &mut self,
        gpu: &GpuCore,
        src: vk::Image,
        src_layout: vk::ImageLayout,
        dst: vk::Image,
        extent: vk::Extent2D,
    ) -> Result<(), RenderError> {
        let inner = self.inner.as_ref().ok_or(RenderError::NoVk)?;
        let vk = Arc::clone(&inner.vk);
        let pool = gpu.ops_command_pool_handle().ok_or(RenderError::NoVk)?;

        crate::kms::vk::ops::run_one_shot_op(&vk, pool, |vk, cb| {
            let full_range = vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1);

            let pre = [
                // src → TRANSFER_SRC_OPTIMAL
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(src_layout)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .image(src)
                    .subresource_range(full_range),
                // dst (UNDEFINED) → TRANSFER_DST_OPTIMAL
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                    .src_access_mask(vk::AccessFlags2::empty())
                    .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .image(dst)
                    .subresource_range(full_range),
            ];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&pre);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };

            let layers = vk::ImageSubresourceLayers::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .layer_count(1);
            let region = [vk::ImageCopy::default()
                .src_subresource(layers)
                .dst_subresource(layers)
                .extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                })];
            unsafe {
                vk.device.cmd_copy_image(
                    cb,
                    src,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &region,
                );
            }

            let post = [vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .image(dst)
                .subresource_range(full_range)];
            let dep = vk::DependencyInfo::default().image_memory_barriers(&post);
            unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };
            Ok(())
        })?;
        Ok(())
    }
}
