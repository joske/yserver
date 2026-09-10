//! Headless tests of the production composite recorder. Never select a GPU.
use super::*;
use crate::kms::vk::{device::VkContext, target::DrawableImage};

fn scene_with_draws(count: usize) -> CompositeScene {
    CompositeScene {
        bg_color: [0.0, 0.0, 1.0, 1.0],
        draws: vec![
            CompositeDraw {
                image_view: vk::ImageView::null(),
                dst_origin: [0.0; 2],
                dst_size: [1.0; 2],
                src_origin: [0.0; 2],
                src_size: [1.0; 2],
                alpha_passthrough: true,
            };
            count
        ],
    }
}

#[test]
fn allocation_failures_never_return_a_recordable_prefix() {
    for count in [1, 1023, 1024, 1025, 4225, 4290] {
        for error in [
            vk::Result::ERROR_OUT_OF_POOL_MEMORY,
            vk::Result::ERROR_OUT_OF_HOST_MEMORY,
            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
        ] {
            let result = allocate_composite_descriptors(&scene_with_draws(count), |requested| {
                assert_eq!(requested, count);
                Err(error)
            });
            assert!(matches!(result, Err(PresentError::Vk(e)) if e == error));
        }
        // Also reject a misbehaving allocator returning a partial success.
        let result = allocate_composite_descriptors(&scene_with_draws(count), |_| {
            Ok(vec![vk::DescriptorSet::null(); count - 1])
        });
        assert!(matches!(result, Err(PresentError::Vk(_))));
    }
    let empty = allocate_composite_descriptors(&scene_with_draws(0), |_| {
        panic!("Vulkan forbids a zero-count allocation")
    })
    .unwrap();
    assert!(empty.is_empty());
}

fn cpu_vk() -> Arc<VkContext> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();
    let vk = VkContext::new().expect("CPU Vulkan required; never silently skip");
    let properties = unsafe {
        vk.instance
            .get_physical_device_properties(vk.physical_device)
    };
    assert_eq!(
        properties.device_type,
        vk::PhysicalDeviceType::CPU,
        "select Lavapipe explicitly with VK_DRIVER_FILES"
    );
    vk
}

struct Offscreen {
    vk: Arc<VkContext>,
    image: DrawableImage,
    pool: vk::CommandPool,
    cb: vk::CommandBuffer,
    readback: vk::Buffer,
    memory: vk::DeviceMemory,
}
impl Offscreen {
    fn new(vk: Arc<VkContext>) -> Self {
        let image = DrawableImage::new_server_owned_window(Arc::clone(&vk), 129, 129).unwrap();
        unsafe {
            let pool = vk
                .device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(vk.graphics_queue_family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .unwrap();
            let cb = vk
                .device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .unwrap()[0];
            let readback = vk
                .device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(129 * 129 * 4)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .unwrap();
            let req = vk.device.get_buffer_memory_requirements(readback);
            let props = vk
                .instance
                .get_physical_device_memory_properties(vk.physical_device);
            let index = (0..props.memory_type_count)
                .find(|i| {
                    req.memory_type_bits & (1 << i) != 0
                        && props.memory_types[*i as usize].property_flags.contains(
                            vk::MemoryPropertyFlags::HOST_VISIBLE
                                | vk::MemoryPropertyFlags::HOST_COHERENT,
                        )
                })
                .expect("host coherent readback memory");
            let memory = vk
                .device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(index),
                    None,
                )
                .unwrap();
            vk.device.bind_buffer_memory(readback, memory, 0).unwrap();
            Self {
                vk,
                image,
                pool,
                cb,
                readback,
                memory,
            }
        }
    }
    fn begin(&self) {
        unsafe {
            self.vk
                .device
                .reset_command_buffer(self.cb, vk::CommandBufferResetFlags::empty())
                .unwrap();
            self.vk
                .device
                .begin_command_buffer(self.cb, &vk::CommandBufferBeginInfo::default())
                .unwrap();
        }
    }
    fn submit(&self) {
        unsafe {
            let cb = [vk::CommandBufferSubmitInfo::default().command_buffer(self.cb)];
            self.vk
                .device
                .queue_submit2(
                    self.vk.graphics_queue,
                    &[vk::SubmitInfo2::default().command_buffer_infos(&cb)],
                    vk::Fence::null(),
                )
                .unwrap();
            self.vk
                .device
                .queue_wait_idle(self.vk.graphics_queue)
                .unwrap();
        }
    }
    fn barrier(
        &self,
        image: vk::Image,
        old: vk::ImageLayout,
        new: vk::ImageLayout,
        src: vk::AccessFlags2,
        dst: vk::AccessFlags2,
    ) {
        let barriers = [vk::ImageMemoryBarrier2::default()
            .image(image)
            .old_layout(old)
            .new_layout(new)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(src)
            .dst_access_mask(dst)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            )];
        unsafe {
            self.vk.device.cmd_pipeline_barrier2(
                self.cb,
                &vk::DependencyInfo::default().image_memory_barriers(&barriers),
            );
        }
    }
    fn initialize_source(&self, source: &DrawableImage) {
        self.begin();
        self.barrier(
            source.vk_image,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags2::empty(),
            vk::AccessFlags2::TRANSFER_WRITE,
        );
        unsafe {
            self.vk.device.cmd_clear_color_image(
                self.cb,
                source.vk_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue {
                    float32: [0.5, 0.0, 0.0, 0.5],
                },
                &[vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1)],
            );
        }
        self.barrier(
            source.vk_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );
        unsafe {
            self.vk.device.end_command_buffer(self.cb).unwrap();
        }
        self.submit();
    }
    fn pixels(&self) -> Vec<u8> {
        self.begin();
        self.barrier(
            self.image.vk_image,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::AccessFlags2::TRANSFER_READ,
        );
        unsafe {
            self.vk.device.cmd_copy_image_to_buffer(
                self.cb,
                self.image.vk_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback,
                &[vk::BufferImageCopy::default()
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width: 129,
                        height: 129,
                        depth: 1,
                    })],
            );
            let barrier = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::TRANSFER)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ)];
            self.vk.device.cmd_pipeline_barrier2(
                self.cb,
                &vk::DependencyInfo::default().memory_barriers(&barrier),
            );
            self.vk.device.end_command_buffer(self.cb).unwrap();
        }
        self.submit();
        unsafe {
            let ptr = self
                .vk
                .device
                .map_memory(self.memory, 0, 129 * 129 * 4, vk::MemoryMapFlags::empty())
                .unwrap();
            let pixels = std::slice::from_raw_parts(ptr.cast::<u8>(), 129 * 129 * 4).to_vec();
            self.vk.device.unmap_memory(self.memory);
            pixels
        }
    }
}
impl ComposeRenderTarget for Offscreen {
    fn image(&self) -> vk::Image {
        self.image.vk_image
    }
    fn image_view(&self) -> vk::ImageView {
        self.image.vk_image_view
    }
    fn command_buffer(&self) -> vk::CommandBuffer {
        self.cb
    }
    fn completion_semaphore(&self) -> vk::Semaphore {
        vk::Semaphore::null()
    }
    fn width(&self) -> u32 {
        129
    }
    fn height(&self) -> u32 {
        129
    }
    fn timestamp_pool(&self) -> vk::QueryPool {
        vk::QueryPool::null()
    }
    fn set_last_gpu_render_ns(&mut self, _: Option<u64>) {}
    fn post_compose_preparation(&self) -> Result<PostComposePreparation, PresentError> {
        Ok(PostComposePreparation::Shared)
    }
    fn record_post_compose(&self, _: &VkContext, _: vk::CommandBuffer, _: PostComposePreparation) {
        self.barrier(
            self.image.vk_image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::GENERAL,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::AccessFlags2::MEMORY_READ,
        );
    }
}

impl Drop for Offscreen {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_command_pool(self.pool, None);
            self.vk.device.destroy_buffer(self.readback, None);
            self.vk.device.free_memory(self.memory, None);
        }
    }
}

#[test]
#[ignore = "requires VK_DRIVER_FILES pointing to a CPU Vulkan ICD"]
fn software_composite_renders_every_shape_piece_and_preserves_holes() {
    let vk = cpu_vk();
    let pipeline = CompositorPipeline::new(Arc::clone(&vk), vk::Format::B8G8R8A8_UNORM).unwrap();
    let source = DrawableImage::new_server_owned_window(Arc::clone(&vk), 1, 1).unwrap();
    let mut rings = [
        CompositePoolRing::new(Arc::clone(&vk), 1024).unwrap(),
        CompositePoolRing::new(Arc::clone(&vk), 1024).unwrap(),
    ];
    let outputs = [
        Offscreen::new(Arc::clone(&vk)),
        Offscreen::new(Arc::clone(&vk)),
    ];
    outputs[0].initialize_source(&source);
    let mut scene = scene_with_draws(4225);
    for (i, draw) in scene.draws.iter_mut().enumerate() {
        draw.image_view = source.vk_image_view;
        draw.dst_origin = [((i % 65) * 2) as f32, ((i / 65) * 2) as f32];
    }
    for round in 0..3 {
        for (output, ring) in outputs.iter().zip(&mut rings) {
            let slot = ring.acquire(scene.draws.len()).unwrap().unwrap();
            let descriptors =
                prepare_composite_descriptors(&vk, &pipeline, ring.pool_at(slot), &scene).unwrap();
            assert_eq!(descriptors.len(), 4225);
            // The guard must reject a prefix before touching command state.
            assert!(
                record_command_buffer(
                    &vk,
                    output,
                    &pipeline,
                    &scene,
                    &descriptors[..1024],
                    Repaint::Full(output.image.extent),
                    &[vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent: output.image.extent
                    }],
                    &[],
                    vk::Pipeline::null(),
                    vk::PipelineLayout::null()
                )
                .is_err()
            );
            record_command_buffer(
                &vk,
                output,
                &pipeline,
                &scene,
                &descriptors,
                Repaint::Full(output.image.extent),
                &[vk::Rect2D {
                    offset: vk::Offset2D::default(),
                    extent: output.image.extent,
                }],
                &[],
                vk::Pipeline::null(),
                vk::PipelineLayout::null(),
            )
            .unwrap();
            output.submit();
            let pixels = output.pixels();
            for (i, pixel) in pixels.chunks_exact(4).enumerate() {
                if i % 129 % 2 == 0 && i / 129 % 2 == 0 {
                    assert!(
                        (127..=128).contains(&pixel[0])
                            && pixel[1] == 0
                            && (127..=128).contains(&pixel[2])
                            && pixel[3] == 255,
                        "round={round} pixel={i} expected one SrcOver blend, got {pixel:?}"
                    );
                } else {
                    assert_eq!(pixel, [255, 0, 0, 255], "hole {i} painted");
                }
            }
            ring.release(slot);
        }
    }
}

/// Replace only vkAllocateDescriptorSets in this test device's dispatch table.
/// Every other Vulkan call uses Lavapipe. No production fault-injection hooks.
unsafe extern "system" fn fail_descriptor_batch(
    _: vk::Device,
    info: *const vk::DescriptorSetAllocateInfo<'_>,
    sets: *mut vk::DescriptorSet,
) -> vk::Result {
    // Match Vulkan's all-or-nothing failure contract.
    unsafe {
        for i in 0..(*info).descriptor_set_count as usize {
            sets.add(i).write(vk::DescriptorSet::null());
        }
    }
    vk::Result::ERROR_OUT_OF_POOL_MEMORY
}

#[test]
#[ignore = "requires VK_DRIVER_FILES pointing to a CPU Vulkan ICD"]
fn software_allocation_failure_prevents_recording_and_submission() {
    let mut vk = cpu_vk();
    let context = Arc::get_mut(&mut vk).unwrap();
    let mut functions = context.device.fp_v1_0().clone();
    functions.allocate_descriptor_sets = fail_descriptor_batch;
    context.device = ash::Device::from_parts_1_3(
        context.device.handle(),
        functions,
        context.device.fp_v1_1().clone(),
        context.device.fp_v1_2().clone(),
        context.device.fp_v1_3().clone(),
    );
    let pipeline = CompositorPipeline::new(Arc::clone(&vk), vk::Format::B8G8R8A8_UNORM).unwrap();
    let mut output = Offscreen::new(Arc::clone(&vk));
    let mut ring = CompositePoolRing::new(Arc::clone(&vk), 1024).unwrap();
    for count in [1, 1023, 1024, 1025, 4225, 4290] {
        let slot = ring.acquire(count).unwrap().unwrap();
        let mut submitted = false;
        let extent = output.image.extent;
        let result = record_and_submit_render(
            &vk,
            &mut output,
            &pipeline,
            ring.pool_at(slot),
            &scene_with_draws(count),
            Repaint::Full(extent),
            &[vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent,
            }],
            vk::Fence::null(),
            &mut submitted,
            &[],
            vk::Pipeline::null(),
            vk::PipelineLayout::null(),
        );
        assert!(matches!(
            result,
            Err(PresentError::Vk(vk::Result::ERROR_OUT_OF_POOL_MEMORY))
        ));
        assert!(
            !submitted,
            "failed frame must not enter the submitted/ack path"
        );
        ring.release(slot);
    }
}
