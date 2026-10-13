//! `PlatformBackend` as the scene's output and cursor-plane sink: thin
//! wrappers over the scanout BO, page-flip, render-completion and cursor-plane
//! methods.

use crate::kms::{
    render::{
        gpu::GpuCore,
        platform::PlatformBackend,
        scene::{
            CursorPlaneSink, CursorShowFailure, FlipRetirement, OutputRoute, OutputSink,
            RenderCompletion, SlotToken,
        },
    },
    vk::scanout::OutputScanout,
};
use ash::vk;
use std::{io, os::fd::OwnedFd};

impl OutputSink for PlatformBackend {
    fn gpu(&self) -> &GpuCore {
        &self.gpu
    }

    fn gpu_mut(&mut self) -> &mut GpuCore {
        &mut self.gpu
    }

    fn cursor_plane(&self) -> &dyn CursorPlaneSink {
        self
    }

    fn cursor_plane_mut(&mut self) -> &mut dyn CursorPlaneSink {
        self
    }

    fn route(&self, output_idx: usize) -> OutputRoute {
        match self.scanout_pools.get(output_idx).and_then(Option::as_ref) {
            Some(OutputScanout::Shared(_)) => OutputRoute::Shared,
            Some(OutputScanout::Copied(_)) => OutputRoute::Copied,
            None => OutputRoute::None,
        }
    }

    fn slot_count(&self, output_idx: usize) -> Option<usize> {
        self.scanout_pools
            .get(output_idx)
            .and_then(|p| p.as_ref().map(|pool| pool.display_pool().bos.len()))
    }

    fn acquire_slot(&mut self, output_idx: usize) -> Option<SlotToken> {
        self.acquire_scanout_bo(output_idx)
    }

    fn slot_image(&self, output_idx: usize, bo_idx: usize) -> Option<vk::Image> {
        match self.scanout_pools.get(output_idx).and_then(|p| p.as_ref()) {
            Some(OutputScanout::Shared(pool)) => pool.bos.get(bo_idx).map(|bo| bo.vk_image),
            Some(OutputScanout::Copied(pool)) => {
                pool.sources.get(bo_idx).map(|source| source.image())
            }
            None => None,
        }
    }

    fn invalidate_slot(&mut self, output_idx: usize, bo_idx: usize) {
        self.invalidate_bo(output_idx, bo_idx);
    }

    fn cancel_slot(&mut self, output_idx: usize, bo_idx: usize) {
        self.cancel_scanout_bo_recording(output_idx, bo_idx);
    }

    fn recycle_failed_slot(&mut self, output_idx: usize, bo_idx: usize) -> io::Result<()> {
        self.recycle_failed_submit_bo(output_idx, bo_idx)
    }

    fn on_page_flip_complete(&mut self, output_idx: usize) -> Option<FlipRetirement> {
        PlatformBackend::on_page_flip_complete(self, output_idx)
    }

    fn commit_slot_present(&mut self, output_idx: usize, bo_idx: usize, generation: u64) {
        self.commit_bo_present(output_idx, bo_idx, generation);
    }

    fn register_render_completion(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        fd: Option<OwnedFd>,
    ) -> io::Result<u64> {
        let output_key = self.outputs[output_idx].key.clone();
        self.register_scanout_render_completion(output_key, bo_idx, fd)
    }

    fn drain_render_completions(&mut self) -> Vec<RenderCompletion> {
        self.drain_scanout_render_completions()
            .into_iter()
            .map(|completion| RenderCompletion {
                job_id: completion.job_id,
                output: self
                    .outputs
                    .iter()
                    .position(|output| output.key == completion.output_key)
                    .ok_or_else(|| format!("{:?}", completion.output_key)),
                bo_idx: completion.bo_idx,
                fd: completion.fd,
            })
            .collect()
    }

    fn clear_render_completions(&mut self) {
        self.clear_scanout_render_completions();
    }

    fn submit_copied(
        &mut self,
        output_idx: usize,
        bo_idx: usize,
        fd: Option<OwnedFd>,
    ) -> io::Result<()> {
        self.submit_copied_scanout(output_idx, bo_idx, fd)
    }
}

impl CursorPlaneSink for PlatformBackend {
    fn fits(&self, output_idx: usize, width: u32, height: u32) -> bool {
        self.cursor_plane_fits_for_output(output_idx, width, height)
    }

    fn visible(&self, output_idx: usize) -> bool {
        self.cursor_plane_visible_for_output(output_idx)
    }

    fn uploaded_version(&self, output_idx: usize) -> Option<u64> {
        self.cursor_plane_uploaded_version_for_output(output_idx)
    }

    fn upload(
        &mut self,
        output_idx: usize,
        version: u64,
        width: u32,
        height: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<()> {
        self.cursor_plane_upload_image_for_output(output_idx, version, width, height, bgra_bytes)
    }

    fn show(
        &mut self,
        output_idx: usize,
        hot_x: u16,
        hot_y: u16,
        x: i32,
        y: i32,
    ) -> Result<(), CursorShowFailure> {
        self.cursor_plane_show_on_crtc(output_idx, hot_x, hot_y, x, y)
            .map_err(|error| CursorShowFailure {
                remains_visible: error.remains_visible(),
                error: Box::new(error),
            })
    }

    fn hide(&mut self, output_idx: usize) -> io::Result<()> {
        self.cursor_plane_hide_on_crtc(output_idx)
    }

    fn hide_all(&mut self) -> io::Result<()> {
        self.cursor_plane_hide_all()
    }

    fn note_composed_retirement(&mut self, output_idx: usize) -> bool {
        self.cursor_plane_note_composed_retirement(output_idx)
    }
}
