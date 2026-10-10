use super::*;

impl DisposableProbeError {
    pub(super) fn quarantined(source: io::Error) -> Self {
        Self {
            source,
            quarantine: true,
            abort_candidate_search: true,
        }
    }

    pub(crate) fn terminal_cleanup(source: io::Error) -> Self {
        Self::terminal_known_quiescent(source)
    }

    fn terminal_known_quiescent(source: io::Error) -> Self {
        Self {
            source,
            quarantine: false,
            abort_candidate_search: true,
        }
    }

    fn with_quarantine(mut self, quarantine: bool) -> Self {
        self.quarantine |= quarantine;
        self.abort_candidate_search |= quarantine;
        self
    }

    pub(super) fn with_context(self, context: impl Into<String>) -> Self {
        Self {
            source: scanout_io_context(context, self.source),
            quarantine: self.quarantine,
            abort_candidate_search: self.abort_candidate_search,
        }
    }

    #[must_use]
    pub(crate) fn requires_quarantine(&self) -> bool {
        self.quarantine
    }

    /// Whether returning this failure through normal RAII could enter an
    /// unbounded device-wide idle or release backing still referenced by a
    /// failed strict DRM cleanup. Submission uncertainty and strict cleanup
    /// failure require quarantine; a pre-submit error and a completed content
    /// mismatch with successful cleanup are safe to tear down normally.
    #[must_use]
    pub(super) fn bypass_normal_teardown(&self) -> bool {
        self.quarantine
    }

    #[must_use]
    pub(crate) fn abort_candidate_search(&self) -> bool {
        self.abort_candidate_search
    }

    #[must_use]
    #[cfg(test)]
    pub(super) fn kind(&self) -> io::ErrorKind {
        self.source.kind()
    }

    #[must_use]
    pub(crate) fn as_io_error(&self) -> &io::Error {
        &self.source
    }

    pub(crate) fn into_io_error_with_context(self, context: impl Into<String>) -> io::Error {
        scanout_io_context(context, self.source)
    }

    pub(super) fn into_io_error(self) -> io::Error {
        self.source
    }
}

impl From<io::Error> for DisposableProbeError {
    fn from(source: io::Error) -> Self {
        Self {
            source,
            quarantine: false,
            abort_candidate_search: false,
        }
    }
}

pub(super) fn completed_probe_validation<T, E>(
    validation: Result<T, E>,
) -> Result<T, DisposableProbeError>
where
    DisposableProbeError: From<E>,
{
    // Once both submitted fences have signalled, content is authoritative.
    // Host-side validation time must never be reclassified as a GPU timeout.
    validation.map_err(DisposableProbeError::from)
}

pub(super) fn finish_disposable_probe_attempt<A>(
    mut attempt: A,
    result: Result<(), DisposableProbeError>,
) -> Result<(), DisposableProbeError>
where
    A: DisposableProbeAttempt,
{
    if result
        .as_ref()
        .is_err_and(DisposableProbeError::bypass_normal_teardown)
    {
        attempt.retain_uncertain();
        return result;
    }

    attempt.mark_known_quiescent();
    if let Err(cleanup) = attempt.release_strict_drm_resources() {
        let prior = result
            .as_ref()
            .err()
            .map_or_else(|| "successful probe".to_string(), ToString::to_string);
        let cleanup = io::Error::new(
            cleanup.kind(),
            format!("strict disposable DRM cleanup failed after {prior}: {cleanup}"),
        );
        attempt.retain_uncertain();
        return Err(DisposableProbeError::quarantined(cleanup));
    }
    drop(attempt);
    result
}

impl DisposableProbeAttempt for ScanoutBoPool {
    fn mark_known_quiescent(&self) {
        for bo in &self.bos {
            bo.vk.mark_disposable_probe_quiescent();
        }
    }

    fn release_strict_drm_resources(&mut self) -> io::Result<()> {
        self.release_disposable_drm_resources()
    }
}

impl DisposableProbeAttempt for CopiedScanoutPool {
    fn mark_known_quiescent(&self) {
        self.mark_disposable_probe_quiescent();
    }

    fn release_strict_drm_resources(&mut self) -> io::Result<()> {
        self.destinations.release_disposable_drm_resources()
    }
}

impl DisposableProbeAttempt for CopiedDisposableProbeAttempt {
    fn mark_known_quiescent(&self) {
        // Mark both contexts before either pool or pipeline Drop observes the
        // policy. Their per-submit fences already prove all child resources are
        // idle, so both destructors may destroy directly.
        self.pool.mark_disposable_probe_quiescent();
    }

    fn release_strict_drm_resources(&mut self) -> io::Result<()> {
        self.pool.destinations.release_disposable_drm_resources()
    }
}

pub(super) fn copied_probe_digest_readback_error(
    operation: &'static str,
    result: vk::Result,
) -> DisposableProbeError {
    let source = scanout_vk_error(operation, result);
    if result == vk::Result::ERROR_DEVICE_LOST {
        // Preserve the structured source chain so the qualification layer can
        // promote this to DeviceLost rather than an indeterminate route.
        DisposableProbeError::from(source)
    } else {
        // Both fences are already complete, so ordinary teardown is safe, but
        // a host mapping/invalidation failure says nothing about route
        // compatibility. Stop candidate search as Indeterminate.
        DisposableProbeError::terminal_known_quiescent(source)
    }
}

pub(super) fn probe_teardown_wait_completed(result: Result<(), vk::Result>) -> bool {
    matches!(result, Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST))
}

impl<'a> ProbeFence<'a> {
    pub(super) fn new(device: &'a ash::Device, handle: vk::Fence) -> Self {
        Self { device, handle }
    }

    pub(super) fn handle(&self) -> vk::Fence {
        self.handle
    }

    pub(super) fn destroy_known_idle(&mut self) {
        if self.handle == vk::Fence::null() {
            return;
        }
        unsafe { self.device.destroy_fence(self.handle, None) };
        self.handle = vk::Fence::null();
    }

    /// Relinquish userspace ownership of a fence whose submission may still
    /// reference it. The aggregate disposable probe attempt keeps the owning
    /// device and every submitted child alive until process exit.
    pub(super) fn abandon_pending(&mut self) {
        self.handle = vk::Fence::null();
    }
}

impl DisposableProbeFence for ProbeFence<'_> {
    fn abandon(&mut self) {
        self.abandon_pending();
    }

    fn destroy_idle(&mut self) {
        self.destroy_known_idle();
    }

    fn wait_bounded(&mut self, timeout_ns: u64, operation: &'static str) -> io::Result<()> {
        match unsafe {
            self.device
                .wait_for_fences(&[self.handle()], true, timeout_ns)
        } {
            Ok(()) => Ok(()),
            Err(vk::Result::TIMEOUT) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{operation} timed out after {:?}",
                    Duration::from_nanos(timeout_ns),
                ),
            )),
            Err(result) => Err(scanout_vk_error(operation, result)),
        }
    }
}

/// Resolve both fence guards without ever waiting device-wide. Returns true
/// when at least one submission remains uncertain and the owning aggregate
/// attempt must also be retained.
#[must_use]
fn dispose_probe_fences_after_failure<R, S>(
    pending: PendingProbeSubmissions,
    render: &mut R,
    sink: &mut S,
) -> bool
where
    R: DisposableProbeFence,
    S: DisposableProbeFence,
{
    match pending {
        PendingProbeSubmissions::None => {
            render.destroy_idle();
            sink.destroy_idle();
            false
        }
        PendingProbeSubmissions::Render => {
            render.abandon();
            sink.destroy_idle();
            true
        }
        PendingProbeSubmissions::RenderAndSink => {
            render.abandon();
            sink.abandon();
            true
        }
    }
}

pub(super) fn finish_pending_probe_failure<R, S>(
    pending: PendingProbeSubmissions,
    error: DisposableProbeError,
    render: &mut R,
    sink: &mut S,
) -> DisposableProbeError
where
    R: DisposableProbeFence,
    S: DisposableProbeFence,
{
    let quarantine = dispose_probe_fences_after_failure(pending, render, sink);
    error.with_quarantine(quarantine)
}

pub(super) fn wait_copy_free_probe_fence<F>(
    fence: &mut F,
    timeout_ns: u64,
) -> Result<(), DisposableProbeError>
where
    F: DisposableProbeFence,
{
    match fence.wait_bounded(timeout_ns, "disposable scanout rendering probe") {
        Ok(()) => {
            fence.destroy_idle();
            Ok(())
        }
        Err(error) => {
            fence.abandon();
            Err(DisposableProbeError::quarantined(error))
        }
    }
}

pub(super) fn wait_copied_probe_fence_pair<R, S>(
    render: &mut R,
    sink: &mut S,
    timeout_ns: u64,
) -> Result<CopiedProbeFenceWaitDurations, DisposableProbeError>
where
    R: DisposableProbeFence,
    S: DisposableProbeFence,
{
    let renderer_started = Instant::now();
    if let Err(error) = render.wait_bounded(timeout_ns, "copied renderer probe") {
        return Err(finish_pending_probe_failure(
            PendingProbeSubmissions::RenderAndSink,
            DisposableProbeError::from(error),
            render,
            sink,
        ));
    }
    render.destroy_idle();
    let renderer_elapsed = renderer_started.elapsed();

    let sink_started = Instant::now();
    if let Err(error) = sink.wait_bounded(timeout_ns, "copied sink probe") {
        sink.abandon();
        return Err(DisposableProbeError::from(error).with_quarantine(true));
    }
    sink.destroy_idle();
    Ok(CopiedProbeFenceWaitDurations {
        renderer: renderer_elapsed,
        sink: sink_started.elapsed(),
    })
}

impl Drop for ProbeFence<'_> {
    fn drop(&mut self) {
        if self.handle == vk::Fence::null() {
            return;
        }
        let wait = unsafe { self.device.device_wait_idle() };
        if !probe_teardown_wait_completed(wait) {
            log::warn!(
                "disposable scanout probe: vkDeviceWaitIdle failed during teardown: {wait:?}; \
                 leaking the uncertain fence"
            );
            self.handle = vk::Fence::null();
            return;
        }
        unsafe { self.device.destroy_fence(self.handle, None) };
        self.handle = vk::Fence::null();
    }
}

pub(super) fn create_probe_fence(vk: &VkContext) -> io::Result<vk::Fence> {
    unsafe {
        vk.device
            .create_fence(&vk::FenceCreateInfo::default(), None)
    }
    .map_err(|result| scanout_vk_error("create copied scanout probe fence", result))
}

pub(super) fn submit_copied_source_probe(
    source: &mut CopiedRenderSource,
    pattern: &CopiedProbePatternPipeline,
    readback: CopiedProbeReadback<'_>,
    frame_token: u32,
    fence: vk::Fence,
) -> Result<Option<OwnedFd>, DisposableProbeError> {
    source.prepare_renderer_acquire()?;
    let transport_preparation = source.transport_preparation()?;
    let device = &source.render_vk.device;
    let command_buffer = source.transfer.command_buffer;
    unsafe {
        device
            .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
            .map_err(|result| {
                scanout_vk_error("reset copied renderer probe command buffer", result)
            })?;
        device
            .begin_command_buffer(
                command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(|result| {
                scanout_vk_error("begin copied renderer probe command buffer", result)
            })?;

        let to_color = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::TOP_OF_PIPE)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .dst_access_mask(vk::AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .image(source.image())
            .subresource_range(color_subresource_range())];
        device.cmd_pipeline_barrier2(
            command_buffer,
            &vk::DependencyInfo::default().image_memory_barriers(&to_color),
        );
        let attachments = [vk::RenderingAttachmentInfo::default()
            .image_view(source.image_view())
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
            })];
        device.cmd_begin_rendering(
            command_buffer,
            &vk::RenderingInfo::default()
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: vk::Extent2D {
                        width: source.width(),
                        height: source.height(),
                    },
                })
                .layer_count(1)
                .color_attachments(&attachments),
        );
        pattern.record(command_buffer, source.width(), source.height(), frame_token);
        device.cmd_end_rendering(command_buffer);
        source.record_transport_copy(command_buffer, transport_preparation);
        source.record_probe_readback(command_buffer, readback);
        device
            .end_command_buffer(command_buffer)
            .map_err(|result| {
                scanout_vk_error("end copied renderer probe command buffer", result)
            })?;

        let waits = source.renderer_wait_semaphore().map(|semaphore| {
            [vk::SemaphoreSubmitInfo::default()
                .semaphore(semaphore)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)]
        });
        let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(command_buffer)];
        let signals = [vk::SemaphoreSubmitInfo::default()
            .semaphore(source.completion_semaphore)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let mut submit = vk::SubmitInfo2::default()
            .command_buffer_infos(&commands)
            .signal_semaphore_infos(&signals);
        if let Some(waits) = waits.as_ref() {
            submit = submit.wait_semaphore_infos(waits);
        }
        let submits = [submit];
        crate::vk_count!(queue_submit2);
        crate::vk_count!(submit_other);
        crate::kms::vk::submit_stats::timed(
            crate::kms::vk::submit_stats::SubmitCause::Scanout,
            1,
            false,
            || device.queue_submit2(source.render_vk.graphics_queue, &submits, fence),
        )
        .map_err(|result| {
            DisposableProbeError::quarantined(scanout_vk_error(
                "submit copied renderer probe",
                result,
            ))
        })?;
    }

    source.note_renderer_submit_succeeded();

    source.export_render_completion().map_err(|result| {
        DisposableProbeError::quarantined(scanout_vk_error(
            "export copied renderer probe completion",
            result,
        ))
    })
}

pub(super) fn tight_bgra_len(width: u32, height: u32) -> io::Result<usize> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| io::Error::other("copied probe BGRA byte length overflow"))?;
    usize::try_from(bytes)
        .map_err(|_| io::Error::other("copied probe BGRA byte length exceeds usize"))
}

pub(super) fn tight_mapped_bgra_bytes(
    transfer: &TransferResources,
    width: u32,
    height: u32,
) -> io::Result<&[u8]> {
    let len = tight_bgra_len(width, height)?;
    if transfer.staging_size < len as u64 {
        return Err(io::Error::other(format!(
            "copied probe staging buffer is too small: have {} bytes, need {len}",
            transfer.staging_size,
        )));
    }
    // SAFETY: `staging_mapped` points to `staging_size` live mapped bytes for
    // the lifetime of `transfer`; the checked slice is no larger than that
    // mapping. Callers read only after the corresponding probe fence signals.
    Ok(unsafe { std::slice::from_raw_parts(transfer.staging_mapped.as_ptr(), len) })
}

pub(super) fn tight_bgra_buffer_image_copy(width: u32, height: u32) -> vk::BufferImageCopy {
    vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(color_subresource_layers())
        .image_extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
}

pub(super) fn probe_buffer_to_host_barrier(
    transfer: &TransferResources,
) -> vk::BufferMemoryBarrier2<'_> {
    vk::BufferMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::HOST)
        .dst_access_mask(vk::AccessFlags2::HOST_READ)
        .buffer(transfer.staging_buffer)
        .offset(0)
        .size(transfer.staging_size)
}

/// Stable FNV-1a digest used only for concise probe diagnostics. Successful
/// validation also compares every byte, so hash collisions cannot admit a
/// corrupt route.
fn copied_probe_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn copied_probe_digest_hash(words: &[u32]) -> u64 {
    words.iter().fold(0xcbf2_9ce4_8422_2325, |hash, word| {
        (hash ^ u64::from(*word)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

pub(super) fn copied_probe_marker_word(rgb: [u8; 3], frame_token: u32) -> u32 {
    u32::from_le_bytes(copied_probe_marker_bgra(rgb, frame_token))
}

pub(super) fn validate_copied_probe_digest_fiducials(
    renderer: &[u32],
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<()> {
    let expected = [
        copied_probe_marker_word([241, 37, 83], frame_token),
        copied_probe_marker_word([29, 211, 71], frame_token),
        copied_probe_marker_word([47, 91, 233], frame_token),
        copied_probe_marker_word([223, 173, 19], frame_token),
    ];
    let actual = renderer.get(..expected.len()).ok_or_else(|| {
        io::Error::other(format!(
            "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: compact GPU \
             digest omitted its four corner fiducials"
        ))
    })?;
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: compact GPU \
                 digest has corner BGRA words {actual:08x?}, expected {expected:08x?}"
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn verify_copied_probe_digests(
    renderer: &[u32],
    sink: &[u32],
    grid_width: u32,
    grid_height: u32,
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<u64> {
    if grid_width == 0 || grid_height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied GPU digest grid must be non-empty",
        ));
    }
    let expected_words = usize::try_from(grid_width)
        .ok()
        .and_then(|width| {
            usize::try_from(grid_height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|blocks| blocks.checked_mul(4))
        .and_then(|digest_words| digest_words.checked_add(4))
        .ok_or_else(|| io::Error::other("copied GPU digest word count overflow"))?;
    if renderer.len() != expected_words || sink.len() != expected_words {
        return Err(io::Error::other(format!(
            "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: unexpected \
             compact GPU digest lengths renderer={} sink={} expected={expected_words}",
            renderer.len(),
            sink.len(),
        )));
    }

    let renderer_hash = copied_probe_digest_hash(renderer);
    let sink_hash = copied_probe_digest_hash(sink);
    if renderer == sink {
        log::debug!(
            "copied content probe compact GPU digest matched: BO {bo_idx} cycle {cycle} token \
             {frame_token} grid={grid_width}x{grid_height} words={expected_words} \
             hash=fnv1a64:{renderer_hash:016x}"
        );
        return Ok(renderer_hash);
    }

    let mismatch = renderer
        .iter()
        .zip(sink)
        .position(|(renderer, sink)| renderer != sink)
        .unwrap_or(0);
    let location = if mismatch < 4 {
        format!("corner={mismatch}")
    } else {
        let digest_word = mismatch - 4;
        let block = digest_word / 4;
        let lane = digest_word % 4;
        let grid_width = usize::try_from(grid_width)
            .map_err(|_| io::Error::other("copied GPU digest grid width exceeds usize"))?;
        format!(
            "block=({}, {}) lane={lane}",
            block % grid_width,
            block / grid_width,
        )
    };
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "copied content probe compact GPU digest mismatch: BO {bo_idx} cycle {cycle} token \
             {frame_token} grid={grid_width}x{grid_height} renderer_hash=fnv1a64:{renderer_hash:016x} \
             sink_hash=fnv1a64:{sink_hash:016x}; first difference at {location}"
        ),
    ))
}

pub(super) fn validate_copied_probe_digest_freshness(
    previous_renderer: Option<&[u32]>,
    renderer: &[u32],
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<()> {
    if previous_renderer.is_some_and(|previous| previous == renderer) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "copied content probe stale compact GPU digest: BO {bo_idx} cycle {cycle} token \
                 {frame_token} repeated the prior renderer frame"
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_copied_probe_fiducials(
    renderer: &[u8],
    width: u32,
    height: u32,
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<()> {
    if width < 2 || height < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copied content probe requires an image at least 2x2",
        ));
    }
    let expected_len = tight_bgra_len(width, height)?;
    if renderer.len() != expected_len {
        return Err(io::Error::other(format!(
            "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: renderer \
             readback length {} does not match expected {expected_len}",
            renderer.len(),
        )));
    }

    // The fragment shader writes exact tokenized RGB corner fiducials.
    // Readback is tightly packed B8G8R8A8, so the byte order below is BGRA.
    // Besides making screenshots orientable, this ensures a failed/no-op draw
    // cannot pass merely because both devices copied the same uniform clear.
    let corners = [
        (0, 0, copied_probe_marker_bgra([241, 37, 83], frame_token)),
        (
            width - 1,
            0,
            copied_probe_marker_bgra([29, 211, 71], frame_token),
        ),
        (
            0,
            height - 1,
            copied_probe_marker_bgra([47, 91, 233], frame_token),
        ),
        (
            width - 1,
            height - 1,
            copied_probe_marker_bgra([223, 173, 19], frame_token),
        ),
    ];
    for (x, y, expected) in corners {
        let start = ((y as usize * width as usize) + x as usize) * 4;
        let actual = &renderer[start..start + 4];
        if actual != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: \
                     renderer fiducial at ({x},{y}) is BGRA {actual:?}, expected {expected:?}"
                ),
            ));
        }
    }
    Ok(())
}

fn copied_probe_marker_bgra(rgb: [u8; 3], frame_token: u32) -> [u8; 4] {
    let token = frame_token as u8;
    let rgb_mask = [token, token.wrapping_mul(17), token.wrapping_mul(31)];
    [
        rgb[2] ^ rgb_mask[2],
        rgb[1] ^ rgb_mask[1],
        rgb[0] ^ rgb_mask[0],
        255,
    ]
}

#[allow(clippy::too_many_arguments)]
pub(super) fn verify_copied_probe_pixels(
    renderer: &[u8],
    sink: &[u8],
    width: u32,
    height: u32,
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<u64> {
    let expected_len = tight_bgra_len(width, height)?;
    if renderer.len() != expected_len || sink.len() != expected_len {
        return Err(io::Error::other(format!(
            "copied content probe BO {bo_idx} cycle {cycle} token {frame_token}: unexpected \
             readback lengths renderer={} sink={} expected={expected_len}",
            renderer.len(),
            sink.len(),
        )));
    }

    let renderer_hash = copied_probe_hash(renderer);
    let sink_hash = copied_probe_hash(sink);
    if renderer_hash == sink_hash && renderer == sink {
        log::debug!(
            "copied content probe matched: BO {bo_idx} cycle {cycle} token {frame_token} \
             {width}x{height} bytes={expected_len} hash=fnv1a64:{renderer_hash:016x}"
        );
        return Ok(renderer_hash);
    }

    let mismatch = renderer
        .iter()
        .zip(sink)
        .position(|(renderer, sink)| renderer != sink)
        .unwrap_or(0);
    let pixel = mismatch / 4;
    let x = pixel % width as usize;
    let y = pixel / width as usize;
    let channel = ["B", "G", "R", "A"][mismatch % 4];
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "copied content probe mismatch: BO {bo_idx} cycle {cycle} token {frame_token} \
             {width}x{height} renderer_hash=fnv1a64:{renderer_hash:016x} \
             sink_hash=fnv1a64:{sink_hash:016x}; first difference at ({x},{y}) channel={channel} \
             renderer={} sink={}",
            renderer[mismatch], sink[mismatch],
        ),
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_copied_probe_freshness(
    previous_renderer_hash: Option<u64>,
    renderer_hash: u64,
    bo_idx: usize,
    cycle: u32,
    frame_token: u32,
) -> io::Result<()> {
    // The shader tokenizes its corner fiducials as well as the radial field,
    // so even the smallest admitted 2x2 extent must change between cycles.
    if previous_renderer_hash == Some(renderer_hash) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "copied content probe BO {bo_idx} produced stale renderer pixels in cycle \
                 {cycle}: frame token {frame_token} repeated fnv1a64:{renderer_hash:016x}"
            ),
        ));
    }
    Ok(())
}
