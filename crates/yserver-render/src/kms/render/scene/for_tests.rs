#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::CursorAssignment;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::DamageAuditTarget;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::OutputSink;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::Repaint;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::ScalePass;
use crate::kms::render::scene::SceneCompositor;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::SceneInput;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::Visibility;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::build::build_scene;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::compose::record_and_submit_render;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::cursor::hw_cursor_allowed;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::damage::clip_rect_to_output_extent;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::damage_audit::create_audit_descriptor_pool;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::output_transform;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::scene::repaint::draw_dst_rect_inward;
#[cfg(any(test, feature = "test-support"))]
use crate::kms::render::store::DrawableStore;
#[cfg(any(test, feature = "test-support"))]
use ash::vk;
#[cfg(any(test, feature = "test-support"))]
use std::sync::Arc;

impl SceneCompositor {
    /// Test-only snapshot of the version currently forcing a per-output
    /// cursor show retry.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests_cursor_retry_version(&self, output_idx: usize) -> Option<u64> {
        self.inner
            .as_ref()
            .and_then(|inner| inner.outputs.get(output_idx))
            .and_then(|output| output.force_show_retry_version)
            .map(|retry| retry.version)
    }

    /// Test-only count of descriptor-pool slots still waiting for their
    /// compose fences to signal.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pending_pool_release_count_for_tests(&self) -> usize {
        self.inner
            .as_ref()
            .map(|inner| {
                inner
                    .outputs
                    .iter()
                    .map(|output| output.pending_pool_releases.len())
                    .sum()
            })
            .unwrap_or_default()
    }

    /// Test-only read-only dump of what scene retirement would consume or
    /// leave behind for one output: every pending acknowledgment (stage, BO,
    /// generation, drawable damage snapshots, submitted output damage), the
    /// output-level damage that survives, the descriptor-pool slots and the
    /// failed-submit BOs. Equal dumps mean retirement touched nothing.
    #[cfg(any(test, feature = "test-support"))]
    pub fn retirement_observation_for_tests(&self, output_idx: usize) -> String {
        let Some(output) = self
            .inner
            .as_ref()
            .and_then(|inner| inner.outputs.get(output_idx))
        else {
            return String::from("no output");
        };
        let acks: Vec<String> = output
            .pending_acks
            .iter()
            .map(|ack| {
                format!(
                    "stage={:?} bo={} generation={} snapshots={:?} output_damage={:?}",
                    ack.stage,
                    ack.bo_idx,
                    ack.generation,
                    ack.drawable_snapshots,
                    ack.submitted_output_damage,
                )
            })
            .collect();
        format!(
            "acks={acks:?} structure_damage={:?} failed_repaint={:?} pool_slots={:?} \
             pending_pool_releases={} failed_submit_bos={} prev_presented={} \
             scanout_damage={:?}",
            output.scene_structure_damage,
            output.pending_repaint_after_failed_submit,
            output.pool_slots,
            output.pending_pool_releases.len(),
            output.failed_submit_bos.len(),
            output.prev_presented.len(),
            output.damage,
        )
    }

    /// Test-only snapshot of the per-output submit retry deadline and the
    /// number of BOs retained after a failed submit.
    #[cfg(any(test, feature = "test-support"))]
    pub fn submit_retry_state_for_tests(&self) -> Vec<(usize, Option<std::time::Instant>, usize)> {
        self.inner
            .as_ref()
            .map(|inner| {
                inner
                    .outputs
                    .iter()
                    .map(|output| {
                        (
                            output.output_idx,
                            output.next_submit_retry_at,
                            output.failed_submit_bos.len(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Test-only: make every compose of `output_idx` report at most `limit`
    /// draws recorded, as an exhausted descriptor pool does.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_compose_descriptor_limit_for_tests(
        &mut self,
        output_idx: usize,
        limit: Option<usize>,
    ) {
        if let Some(output) = self
            .inner
            .as_mut()
            .and_then(|inner| inner.outputs.get_mut(output_idx))
        {
            output.test_compose_descriptor_sets = limit;
        }
    }

    /// Test-only: force [`has_pending_page_flips`](Self::has_pending_page_flips)
    /// without a live output/`PendingAck` queue.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_set_flip_in_flight(&mut self, value: bool) {
        self.test_flip_in_flight_override = Some(value);
    }
}

impl SceneCompositor {
    /// Test-only: the cursor assignment the production tick would make for
    /// `output_idx`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn cursor_assignment_for_tests(
        &self,
        input: &SceneInput,
        store: &mut DrawableStore,
        sink: &dyn OutputSink,
        output_idx: usize,
    ) -> CursorAssignment {
        let cursor = self.inner.as_ref().and_then(|inner| inner.cursor.clone());
        build_scene(
            input,
            store,
            output_idx,
            sink,
            cursor,
            None,
            None,
            hw_cursor_allowed(input.outputs) && !input.hw_cursor_suppressed,
            Visibility::On,
        )
        .cursor_assignment
    }

    /// Test-only: output `output_idx`'s intermediate memory and extent.
    #[cfg(any(test, feature = "test-support"))]
    pub fn intermediate_for_tests(
        &self,
        output_idx: usize,
    ) -> Option<(vk::DeviceMemory, vk::Extent2D)> {
        let im = self
            .inner
            .as_ref()?
            .outputs
            .get(output_idx)?
            .intermediate
            .as_ref()?;
        Some((im.memory(), im.extent))
    }

    /// Test-only: compose `output_idx` exactly as `tick_one_output` does for a
    /// transformed output — the scene walk into its live intermediate, then
    /// the scale pass — with a plain offscreen image standing in for the
    /// scanout BO (lavapipe cannot allocate one). Returns that image's BGRA
    /// bytes, mode-sized.
    #[cfg(any(test, feature = "test-support"))]
    pub fn compose_transformed_for_tests(
        &mut self,
        input: &SceneInput,
        store: &mut DrawableStore,
        sink: &dyn OutputSink,
        output_idx: usize,
    ) -> Vec<u8> {
        self.compose_for_tests(input, store, sink, output_idx).0
    }

    /// Test-only: compose identity output `output_idx` as `tick_one_output`
    /// does, into an offscreen stand-in for its scanout BO. Returns the BO's
    /// bytes and the same bytes as a root read of the BO sees them.
    #[cfg(any(test, feature = "test-support"))]
    pub fn compose_identity_for_tests(
        &mut self,
        input: &SceneInput,
        store: &mut DrawableStore,
        sink: &dyn OutputSink,
        output_idx: usize,
    ) -> (Vec<u8>, Vec<u8>) {
        let (bytes, image, extent) = self.compose_for_tests(input, store, sink, output_idx);
        let mut read = bytes.clone();
        self.restore_under_cursor(
            image,
            vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent,
            },
            &mut read,
        );
        (bytes, read)
    }

    /// The shared body of the two helpers above: the compose target's bytes,
    /// the image the cursor save is keyed by, and the target extent.
    #[cfg(any(test, feature = "test-support"))]
    fn compose_for_tests(
        &mut self,
        input: &SceneInput,
        store: &mut DrawableStore,
        sink: &dyn OutputSink,
        output_idx: usize,
    ) -> (Vec<u8>, vk::Image, vk::Extent2D) {
        let inner = self.inner.as_mut().expect("live scene");
        let transform = output_transform(input.outputs, output_idx).cloned();
        let cursor = inner.cursor.clone();
        let built = build_scene(
            input,
            store,
            output_idx,
            sink,
            cursor,
            None,
            None,
            false,
            Visibility::On,
        );
        let layout = &input.outputs[output_idx];
        let mode = vk::Extent2D {
            width: u32::from(layout.mode.0),
            height: u32::from(layout.mode.1),
        };
        let mut target = DamageAuditTarget::new(Arc::clone(&inner.vk), mode).expect("target");
        let state = &mut inner.outputs[output_idx];
        let scale = transform.as_ref().map(|transform| {
            ScalePass::new(
                state.intermediate.as_ref().expect("intermediate"),
                inner.scale_pipeline.as_ref().expect("scale pipeline"),
                transform,
                layout.root_rect,
                (u32::from(input.fb_size.0), u32::from(input.fb_size.1)),
            )
        });
        let (compose_image, compose_extent) =
            scale.map_or((target.image, mode), |sp| (sp.image, sp.extent));
        let cursor_rect = built
            .software_cursor_tail
            .and(built.scene.draws.last())
            .and_then(draw_dst_rect_inward)
            .and_then(|rect| clip_rect_to_output_extent(rect, compose_extent));
        let vk = &inner.vk;
        let cursor_save = state.cursor_saves.prepare(vk, compose_image, cursor_rect);
        let pool = create_audit_descriptor_pool(vk, built.scene.draws.len()).expect("pool");
        let ticket = sink.gpu().acquire_fence_ticket().expect("fence");
        let mut submitted = false;
        record_and_submit_render(
            vk,
            &mut target,
            &inner.pipeline,
            pool,
            &built.scene,
            Repaint::Full(compose_extent),
            &[],
            ticket.fence(),
            &mut submitted,
            &[],
            vk::Pipeline::null(),
            vk::PipelineLayout::null(),
            scale.as_ref(),
            cursor_save,
        )
        .expect("compose");
        ticket.wait(vk).expect("compose fence");
        state
            .cursor_saves
            .finish(compose_image, cursor_save, Some(&ticket));
        if let Some(intermediate) = state.intermediate.as_mut() {
            intermediate.has_content = true;
        }
        unsafe { vk.device.destroy_descriptor_pool(pool, None) };
        (
            read_general_image_for_tests(vk, sink, target.image, mode),
            target.image,
            mode,
        )
    }
}

/// Test-only: the BGRA bytes of a `GENERAL`-layout colour image.
#[cfg(any(test, feature = "test-support"))]
fn read_general_image_for_tests(
    vk: &Arc<crate::kms::vk::device::VkContext>,
    sink: &dyn OutputSink,
    image: vk::Image,
    extent: vk::Extent2D,
) -> Vec<u8> {
    let bytes = u64::from(extent.width) * u64::from(extent.height) * 4;
    let staging =
        crate::kms::render::engine::StagingBuffer::new_for_readback(Arc::clone(vk), bytes)
            .expect("staging");
    let mut op = crate::kms::vk::ops::ReusableOneShot::new(
        Arc::clone(vk),
        sink.gpu().ops_command_pool_handle().expect("ops pool"),
    )
    .expect("one-shot");
    let buffer = staging.buffer();
    op.run(|vk, cb| {
        let color = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .image(image)
            .subresource_range(color)];
        let region = [vk::BufferImageCopy::default()
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            })];
        unsafe {
            vk.device.cmd_pipeline_barrier2(
                cb,
                &vk::DependencyInfo::default().image_memory_barriers(&barrier),
            );
            vk.device.cmd_copy_image_to_buffer(
                cb,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &region,
            );
        }
        Ok(())
    })
    .map_err(|e| e.result)
    .expect("readback");
    staging.invalidate_for_read().expect("invalidate");
    let len = usize::try_from(bytes).expect("size");
    // SAFETY: mapped for `bytes`, the copy's fence has signalled.
    unsafe { std::slice::from_raw_parts(staging.mapped().as_ptr(), len) }.to_vec()
}
