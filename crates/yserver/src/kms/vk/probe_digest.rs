//! GPU-side summaries for copied reverse-PRIME content probes.
//!
//! A route probe still copies the complete image into device-local storage on
//! each Vulkan device. Compute reduces that storage to a fixed-size grid of
//! positional hashes, so validation maps and compares at most about 64 KiB
//! instead of reading a full scanout image through a potentially slow BAR.

use std::{ptr::NonNull, sync::Arc};

use ash::vk;

use super::device::VkContext;

const COMPUTE_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/probe_digest.comp.spv"));

const MAX_GRID_WIDTH: u32 = 64;
const MAX_GRID_HEIGHT: u32 = 64;
const LOCAL_SIZE_X: u32 = 64;
const CORNER_WORDS: u64 = 4;
const HASH_WORDS_PER_BLOCK: u64 = 4;
const BYTES_PER_PIXEL: u64 = 4;
const BYTES_PER_WORD: u64 = 4;

/// Matches the compute shader's push-constant block exactly.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProbeDigestPushConstants {
    extent: [u32; 2],
    grid: [u32; 2],
}

const _: () = assert!(std::mem::size_of::<ProbeDigestPushConstants>() == 16);
const _: () = assert!(std::mem::offset_of!(ProbeDigestPushConstants, grid) == 8);

impl ProbeDigestPushConstants {
    const fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` plus the size/offset assertions above establish a
        // fully initialized 16-byte layout shared with the GLSL block.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref::<Self>(self).cast::<u8>(),
                std::mem::size_of::<Self>(),
            )
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProbeDigestLayout {
    push_constants: ProbeDigestPushConstants,
    input_bytes: u64,
    summary_words: usize,
    summary_bytes: u64,
    dispatch_x: u32,
}

impl ProbeDigestLayout {
    fn new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }

        let input_bytes = u64::from(width)
            .checked_mul(u64::from(height))?
            .checked_mul(BYTES_PER_PIXEL)?;
        let grid_width = width.min(MAX_GRID_WIDTH);
        let grid_height = height.min(MAX_GRID_HEIGHT);
        let block_count = u64::from(grid_width).checked_mul(u64::from(grid_height))?;
        let summary_words_u64 =
            CORNER_WORDS.checked_add(block_count.checked_mul(HASH_WORDS_PER_BLOCK)?)?;
        let summary_words = usize::try_from(summary_words_u64).ok()?;
        let summary_bytes = summary_words_u64.checked_mul(BYTES_PER_WORD)?;
        let block_count_u32 = u32::try_from(block_count).ok()?;
        let dispatch_x = block_count_u32.div_ceil(LOCAL_SIZE_X);

        Some(Self {
            push_constants: ProbeDigestPushConstants {
                extent: [width, height],
                grid: [grid_width, grid_height],
            },
            input_bytes,
            summary_words,
            summary_bytes,
            dispatch_x,
        })
    }
}

/// One compute pipeline and its private input/output buffers for a Vulkan
/// context participating in a copied content probe.
pub(crate) struct ProbeDigestPipeline {
    vk: Arc<VkContext>,
    layout: ProbeDigestLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    input_buffer: vk::Buffer,
    input_memory: vk::DeviceMemory,
    output_buffer: vk::Buffer,
    output_memory: vk::DeviceMemory,
    output_mapped: NonNull<u32>,
    output_coherent: bool,
}

impl ProbeDigestPipeline {
    /// Whether this context can reduce a tightly packed BGRA image of the
    /// requested extent through its selected queue and storage-buffer limits.
    #[must_use]
    pub(crate) fn is_supported(vk: &VkContext, width: u32, height: u32) -> bool {
        let Some(layout) = ProbeDigestLayout::new(width, height) else {
            return false;
        };
        vk.graphics_queue_supports_compute()
            && layout.input_bytes <= vk.max_storage_buffer_range()
            && layout.summary_bytes <= vk.max_storage_buffer_range()
    }

    /// Allocate one full-size device-local input and one compact mapped output,
    /// then bind both to a context-local compute pipeline.
    pub(crate) fn new(vk: Arc<VkContext>, width: u32, height: u32) -> Result<Self, vk::Result> {
        let layout =
            ProbeDigestLayout::new(width, height).ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        if !Self::is_supported(&vk, width, height) {
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        }

        let mut construction = ProbeDigestConstruction::new(&vk);

        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let descriptor_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        construction.handles.descriptor_set_layout = unsafe {
            vk.device
                .create_descriptor_set_layout(&descriptor_layout_info, None)?
        };

        let descriptor_layouts = [construction.handles.descriptor_set_layout];
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(std::mem::size_of::<ProbeDigestPushConstants>() as u32)];
        let pipeline_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&descriptor_layouts)
            .push_constant_ranges(&push_constant_ranges);
        construction.handles.pipeline_layout = unsafe {
            vk.device
                .create_pipeline_layout(&pipeline_layout_info, None)?
        };
        construction.handles.pipeline =
            build_pipeline(&vk.device, construction.handles.pipeline_layout)?;

        let input = allocate_buffer(
            &vk,
            layout.input_bytes,
            vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::STORAGE_BUFFER,
            BufferMemoryKind::DeviceLocal,
        )?;
        construction.handles.input_buffer = input.buffer;
        construction.handles.input_memory = input.memory;

        let output = allocate_buffer(
            &vk,
            layout.summary_bytes,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            BufferMemoryKind::HostCached,
        )?;
        construction.handles.output_buffer = output.buffer;
        construction.handles.output_memory = output.memory;
        construction.handles.output_coherent = output
            .memory_properties
            .contains(vk::MemoryPropertyFlags::HOST_COHERENT);

        let mapped = unsafe {
            vk.device.map_memory(
                output.memory,
                0,
                vk::WHOLE_SIZE,
                vk::MemoryMapFlags::empty(),
            )?
        };
        construction.handles.output_mapped =
            Some(NonNull::new(mapped.cast::<u32>()).expect("vkMapMemory returned a null pointer"));

        let pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::STORAGE_BUFFER,
            descriptor_count: 2,
        }];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        construction.handles.descriptor_pool =
            unsafe { vk.device.create_descriptor_pool(&pool_info, None)? };

        let set_layouts = [construction.handles.descriptor_set_layout];
        let set_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(construction.handles.descriptor_pool)
            .set_layouts(&set_layouts);
        construction.handles.descriptor_set =
            unsafe { vk.device.allocate_descriptor_sets(&set_info)?[0] };

        let input_info = [vk::DescriptorBufferInfo::default()
            .buffer(input.buffer)
            .offset(0)
            .range(layout.input_bytes)];
        let output_info = [vk::DescriptorBufferInfo::default()
            .buffer(output.buffer)
            .offset(0)
            .range(layout.summary_bytes)];
        let descriptor_writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(construction.handles.descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&input_info),
            vk::WriteDescriptorSet::default()
                .dst_set(construction.handles.descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&output_info),
        ];
        unsafe {
            vk.device.update_descriptor_sets(&descriptor_writes, &[]);
        }

        let handles = construction.finish();
        Ok(Self {
            vk,
            layout,
            descriptor_set_layout: handles.descriptor_set_layout,
            descriptor_pool: handles.descriptor_pool,
            descriptor_set: handles.descriptor_set,
            pipeline_layout: handles.pipeline_layout,
            pipeline: handles.pipeline,
            input_buffer: handles.input_buffer,
            input_memory: handles.input_memory,
            output_buffer: handles.output_buffer,
            output_memory: handles.output_memory,
            output_mapped: handles
                .output_mapped
                .expect("successful digest construction maps its output"),
            output_coherent: handles.output_coherent,
        })
    }

    /// Device-local destination for the caller's tightly packed
    /// `vkCmdCopyImageToBuffer` operation.
    #[must_use]
    pub(crate) const fn input_buffer(&self) -> vk::Buffer {
        self.input_buffer
    }

    #[must_use]
    pub(crate) const fn grid_width(&self) -> u32 {
        self.layout.push_constants.grid[0]
    }

    #[must_use]
    pub(crate) const fn grid_height(&self) -> u32 {
        self.layout.push_constants.grid[1]
    }

    #[must_use]
    pub(crate) const fn summary_word_count(&self) -> usize {
        self.layout.summary_words
    }

    /// Record the transfer-write → compute-read dependency, dispatch the
    /// digest, and make its compact output visible to the host. The caller must
    /// record a complete tightly packed image-to-`input_buffer()` copy first.
    pub(crate) fn record_after_transfer(&self, command_buffer: vk::CommandBuffer) {
        let to_compute = [transfer_to_compute_barrier(
            self.input_buffer,
            self.layout.input_bytes,
        )];
        let to_compute_dependency =
            vk::DependencyInfo::default().buffer_memory_barriers(&to_compute);

        let to_host = [compute_to_host_barrier(
            self.output_buffer,
            self.layout.summary_bytes,
        )];
        let to_host_dependency = vk::DependencyInfo::default().buffer_memory_barriers(&to_host);

        unsafe {
            self.vk
                .device
                .cmd_pipeline_barrier2(command_buffer, &to_compute_dependency);
            self.vk.device.cmd_bind_pipeline(
                command_buffer,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline,
            );
            self.vk.device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::COMPUTE,
                self.pipeline_layout,
                0,
                &[self.descriptor_set],
                &[],
            );
            self.vk.device.cmd_push_constants(
                command_buffer,
                self.pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                self.layout.push_constants.as_bytes(),
            );
            self.vk
                .device
                .cmd_dispatch(command_buffer, self.layout.dispatch_x, 1, 1);
            self.vk
                .device
                .cmd_pipeline_barrier2(command_buffer, &to_host_dependency);
        }
    }

    /// Read the four raw corner words followed by four hash words per block.
    ///
    /// The caller must wait for the submission fence before calling this. The
    /// compact mapped range is copied into an owned vector so safe callers
    /// cannot retain an alias while a later submission overwrites the output.
    pub(crate) fn read_summary(&self) -> Result<Vec<u32>, vk::Result> {
        if !self.output_coherent {
            let ranges = [vk::MappedMemoryRange::default()
                .memory(self.output_memory)
                .offset(0)
                .size(vk::WHOLE_SIZE)];
            unsafe {
                self.vk.device.invalidate_mapped_memory_ranges(&ranges)?;
            }
        }

        // SAFETY: construction maps at least `summary_words * 4` bytes and the
        // mapping remains live until Drop. The fence precondition above makes
        // the GPU writes complete and the invalidate handles non-coherent
        // memory before the temporary host view is copied.
        Ok(unsafe {
            std::slice::from_raw_parts(self.output_mapped.as_ptr(), self.layout.summary_words)
        }
        .to_vec())
    }
}

impl Drop for ProbeDigestPipeline {
    fn drop(&mut self) {
        // Match the diagnostic graphics pipeline's conservative policy. A
        // successfully finalized disposable probe marks the context quiescent;
        // an unexpected unwind waits before destroying potentially referenced
        // children. If that wait itself fails (except device loss), leak both
        // the children and a context Arc rather than risk driver use-after-free.
        if self.vk.requires_drop_device_idle() {
            let wait = unsafe { self.vk.device.device_wait_idle() };
            if !matches!(wait, Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST)) {
                log::warn!(
                    "copied content-probe digest: vkDeviceWaitIdle failed during teardown: \
                     {wait:?}; leaking the uncertain digest resources and device context"
                );
                std::mem::forget(Arc::clone(&self.vk));
                return;
            }
        }

        unsafe {
            self.vk.device.unmap_memory(self.output_memory);
            destroy_handles(
                &self.vk.device,
                &mut ProbeDigestHandles {
                    descriptor_set_layout: self.descriptor_set_layout,
                    descriptor_pool: self.descriptor_pool,
                    descriptor_set: self.descriptor_set,
                    pipeline_layout: self.pipeline_layout,
                    pipeline: self.pipeline,
                    input_buffer: self.input_buffer,
                    input_memory: self.input_memory,
                    output_buffer: self.output_buffer,
                    output_memory: self.output_memory,
                    output_mapped: None,
                    output_coherent: self.output_coherent,
                },
            );
        }
    }
}

fn transfer_to_compute_barrier(buffer: vk::Buffer, size: u64) -> vk::BufferMemoryBarrier2<'static> {
    vk::BufferMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COPY)
        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_READ)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .buffer(buffer)
        .offset(0)
        .size(size)
}

fn compute_to_host_barrier(buffer: vk::Buffer, size: u64) -> vk::BufferMemoryBarrier2<'static> {
    vk::BufferMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
        .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags2::HOST)
        .dst_access_mask(vk::AccessFlags2::HOST_READ)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .buffer(buffer)
        .offset(0)
        .size(size)
}

#[derive(Clone, Copy)]
enum BufferMemoryKind {
    DeviceLocal,
    HostCached,
}

#[derive(Clone, Copy)]
struct AllocatedBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    memory_properties: vk::MemoryPropertyFlags,
}

fn allocate_buffer(
    vk: &VkContext,
    size: u64,
    usage: vk::BufferUsageFlags,
    memory_kind: BufferMemoryKind,
) -> Result<AllocatedBuffer, vk::Result> {
    let create_info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { vk.device.create_buffer(&create_info, None)? };
    let requirements = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
    let memory_properties = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };

    let memory_type_index = pick_buffer_memory_type(
        &memory_properties,
        requirements.memory_type_bits,
        memory_kind,
    )
    .ok_or(vk::Result::ERROR_FEATURE_NOT_PRESENT);
    let memory_type_index = match memory_type_index {
        Ok(index) => index,
        Err(error) => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(error);
        }
    };
    let selected_properties =
        memory_properties.memory_types[memory_type_index as usize].property_flags;

    let allocation_info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_type_index);
    let memory = match unsafe { vk.device.allocate_memory(&allocation_info, None) } {
        Ok(memory) => memory,
        Err(error) => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(error);
        }
    };
    if let Err(error) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_buffer(buffer, None);
        }
        return Err(error);
    }

    Ok(AllocatedBuffer {
        buffer,
        memory,
        memory_properties: selected_properties,
    })
}

fn pick_buffer_memory_type(
    properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    kind: BufferMemoryKind,
) -> Option<u32> {
    let preference_sets: &[vk::MemoryPropertyFlags] = match kind {
        BufferMemoryKind::DeviceLocal => &[vk::MemoryPropertyFlags::DEVICE_LOCAL],
        BufferMemoryKind::HostCached => &[
            vk::MemoryPropertyFlags::HOST_VISIBLE
                | vk::MemoryPropertyFlags::HOST_CACHED
                | vk::MemoryPropertyFlags::HOST_COHERENT,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            vk::MemoryPropertyFlags::HOST_VISIBLE,
        ],
    };

    preference_sets.iter().find_map(|required| {
        (0..properties.memory_type_count).find(|&index| {
            type_bits & (1 << index) != 0
                && properties.memory_types[index as usize]
                    .property_flags
                    .contains(*required)
        })
    })
}

fn build_pipeline(
    device: &ash::Device,
    pipeline_layout: vk::PipelineLayout,
) -> Result<vk::Pipeline, vk::Result> {
    let shader_module = create_shader_module(device, COMPUTE_SPV)?;
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(shader_module)
        .name(c"main");
    let pipeline_info = vk::ComputePipelineCreateInfo::default()
        .stage(stage)
        .layout(pipeline_layout);
    let result = unsafe {
        device.create_compute_pipelines(vk::PipelineCache::null(), &[pipeline_info], None)
    };
    unsafe { device.destroy_shader_module(shader_module, None) };

    match result {
        Ok(pipelines) => Ok(pipelines[0]),
        Err((pipelines, error)) => {
            unsafe {
                for pipeline in pipelines {
                    device.destroy_pipeline(pipeline, None);
                }
            }
            Err(error)
        }
    }
}

fn create_shader_module(
    device: &ash::Device,
    spirv_bytes: &[u8],
) -> Result<vk::ShaderModule, vk::Result> {
    debug_assert!(spirv_bytes.len().is_multiple_of(4));
    let code: Vec<u32> = spirv_bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    unsafe { device.create_shader_module(&info, None) }
}

#[derive(Clone, Copy, Default)]
struct ProbeDigestHandles {
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    input_buffer: vk::Buffer,
    input_memory: vk::DeviceMemory,
    output_buffer: vk::Buffer,
    output_memory: vk::DeviceMemory,
    output_mapped: Option<NonNull<u32>>,
    output_coherent: bool,
}

struct ProbeDigestConstruction<'a> {
    vk: &'a VkContext,
    handles: ProbeDigestHandles,
    armed: bool,
}

impl<'a> ProbeDigestConstruction<'a> {
    fn new(vk: &'a VkContext) -> Self {
        Self {
            vk,
            handles: ProbeDigestHandles::default(),
            armed: true,
        }
    }

    fn finish(mut self) -> ProbeDigestHandles {
        self.armed = false;
        self.handles
    }
}

impl Drop for ProbeDigestConstruction<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        unsafe {
            if self.handles.output_mapped.is_some() {
                self.vk.device.unmap_memory(self.handles.output_memory);
                self.handles.output_mapped = None;
            }
            destroy_handles(&self.vk.device, &mut self.handles);
        }
    }
}

unsafe fn destroy_handles(device: &ash::Device, handles: &mut ProbeDigestHandles) {
    if handles.pipeline != vk::Pipeline::null() {
        unsafe { device.destroy_pipeline(handles.pipeline, None) };
        handles.pipeline = vk::Pipeline::null();
    }
    if handles.pipeline_layout != vk::PipelineLayout::null() {
        unsafe { device.destroy_pipeline_layout(handles.pipeline_layout, None) };
        handles.pipeline_layout = vk::PipelineLayout::null();
    }
    if handles.descriptor_pool != vk::DescriptorPool::null() {
        unsafe { device.destroy_descriptor_pool(handles.descriptor_pool, None) };
        handles.descriptor_pool = vk::DescriptorPool::null();
        handles.descriptor_set = vk::DescriptorSet::null();
    }
    if handles.descriptor_set_layout != vk::DescriptorSetLayout::null() {
        unsafe { device.destroy_descriptor_set_layout(handles.descriptor_set_layout, None) };
        handles.descriptor_set_layout = vk::DescriptorSetLayout::null();
    }
    if handles.input_buffer != vk::Buffer::null() {
        unsafe { device.destroy_buffer(handles.input_buffer, None) };
        handles.input_buffer = vk::Buffer::null();
    }
    if handles.input_memory != vk::DeviceMemory::null() {
        unsafe { device.free_memory(handles.input_memory, None) };
        handles.input_memory = vk::DeviceMemory::null();
    }
    if handles.output_buffer != vk::Buffer::null() {
        unsafe { device.destroy_buffer(handles.output_buffer, None) };
        handles.output_buffer = vk::Buffer::null();
    }
    if handles.output_memory != vk::DeviceMemory::null() {
        unsafe { device.free_memory(handles.output_memory, None) };
        handles.output_memory = vk::DeviceMemory::null();
    }
}

#[cfg(test)]
mod tests {
    use ash::vk::Handle as _;

    use super::*;

    const COMPUTE_GLSL: &str = include_str!("shaders/probe_digest.comp.glsl");

    fn block_bounds(extent: u32, grid: u32, block: u32) -> (u32, u32) {
        let base = extent / grid;
        let extra = extent % grid;
        let start = block * base + block.min(extra);
        let end = start + base + u32::from(block < extra);
        (start, end)
    }

    /// Scalar reference for the exact GLSL partition, constants, operation
    /// order, and wrapping-u32 arithmetic. Deliberately compiled only in tests:
    /// production validation must not fall back to reading the full image.
    fn reference_digest(pixels: &[u32], width: u32, height: u32) -> Vec<u32> {
        let layout = ProbeDigestLayout::new(width, height).unwrap();
        assert_eq!(pixels.len(), width as usize * height as usize);
        let grid_width = layout.push_constants.grid[0];
        let grid_height = layout.push_constants.grid[1];
        let mut summary = vec![0; layout.summary_words];
        let bottom = (height - 1) * width;
        summary[..4].copy_from_slice(&[
            pixels[0],
            pixels[(width - 1) as usize],
            pixels[bottom as usize],
            pixels[(bottom + width - 1) as usize],
        ]);

        for block_y in 0..grid_height {
            let (y0, y1) = block_bounds(height, grid_height, block_y);
            for block_x in 0..grid_width {
                let (x0, x1) = block_bounds(width, grid_width, block_x);
                let block_id = block_y * grid_width + block_x;
                let mut hash = [
                    0x811c_9dc5 ^ block_id,
                    0x9e37_79b9 ^ block_id.wrapping_mul(0x85eb_ca6b),
                    0x243f_6a88 ^ block_id.wrapping_mul(0xc2b2_ae35),
                    0xb7e1_5162 ^ block_id.wrapping_mul(0x27d4_eb2d),
                ];

                for y in y0..y1 {
                    let row = y * width;
                    for x in x0..x1 {
                        let position = row + x;
                        let pixel = pixels[position as usize];
                        let keyed = pixel ^ position.wrapping_mul(0x9e37_79b9);
                        hash[0] = (hash[0] ^ keyed).wrapping_mul(0x0100_0193);
                        hash[1] = hash[1]
                            .wrapping_add(pixel)
                            .wrapping_add(position.wrapping_mul(0x85eb_ca6b))
                            .rotate_left(11)
                            .wrapping_mul(0xc2b2_ae35);
                        hash[2] = (hash[2]
                            ^ pixel.wrapping_add(position.wrapping_mul(0x27d4_eb2d)))
                        .rotate_left(13)
                        .wrapping_mul(0x1656_67b1);
                        hash[3] = hash[3]
                            .wrapping_add(pixel ^ position.rotate_left(16))
                            .rotate_left(17)
                            .wrapping_mul(0x85eb_ca6b);
                    }
                }

                let offset = 4 + block_id as usize * 4;
                summary[offset..offset + 4].copy_from_slice(&hash);
            }
        }
        summary
    }

    fn block_for_pixel(width: u32, height: u32, x: u32, y: u32) -> usize {
        let layout = ProbeDigestLayout::new(width, height).unwrap();
        let grid_width = layout.push_constants.grid[0];
        let grid_height = layout.push_constants.grid[1];
        let block_x = (0..grid_width)
            .find(|&block| {
                let (start, end) = block_bounds(width, grid_width, block);
                (start..end).contains(&x)
            })
            .unwrap();
        let block_y = (0..grid_height)
            .find(|&block| {
                let (start, end) = block_bounds(height, grid_height, block);
                (start..end).contains(&y)
            })
            .unwrap();
        (block_y * grid_width + block_x) as usize
    }

    #[test]
    fn push_constants_and_large_layout_match_shader_contract() {
        assert_eq!(std::mem::size_of::<ProbeDigestPushConstants>(), 16);
        assert_eq!(std::mem::offset_of!(ProbeDigestPushConstants, grid), 8);

        let layout = ProbeDigestLayout::new(1600, 1200).unwrap();
        assert_eq!(layout.push_constants.extent, [1600, 1200]);
        assert_eq!(layout.push_constants.grid, [64, 64]);
        assert_eq!(layout.input_bytes, 7_680_000);
        assert_eq!(layout.summary_words, 4 + 64 * 64 * 4);
        assert_eq!(layout.summary_bytes, 65_552);
        assert_eq!(layout.dispatch_x, 64);
    }

    #[test]
    fn small_images_use_only_nonempty_grid_blocks() {
        let layout = ProbeDigestLayout::new(3, 2).unwrap();
        assert_eq!(layout.push_constants.grid, [3, 2]);
        assert_eq!(layout.summary_words, 4 + 3 * 2 * 4);
        assert!(ProbeDigestLayout::new(0, 2).is_none());
        assert!(ProbeDigestLayout::new(3, 0).is_none());
    }

    #[test]
    fn scalar_reference_has_fixed_odd_extent_vector() {
        let (width, height): (u32, u32) = (67, 65);
        let pixels: Vec<u32> = (0..width * height)
            .map(|position| 0xff00_0000 ^ position.wrapping_mul(0x0001_0203))
            .collect();
        let summary = reference_digest(&pixels, width, height);

        assert_eq!(
            &summary[..4],
            &[0xff00_0000, 0xff42_84c6, 0xefe1_b240, 0xee24_3706]
        );
        assert_eq!(
            &summary[4..8],
            &[0xbb9e_c45d, 0x5d5a_37cf, 0x8e7a_7499, 0x51c2_c690]
        );
        assert_eq!(
            &summary[summary.len() - 4..],
            &[0x42e5_55ca, 0x749a_de03, 0xa5da_7fec, 0xff25_2457]
        );
    }

    #[test]
    fn scalar_reference_tracks_shader_partition_and_hash_body() {
        let partition = r#"    uint x0 = block_x * base_width + min(block_x, extra_width);
    uint x1 = x0 + base_width + uint(block_x < extra_width);

    uint base_height = height / params.grid.y;
    uint extra_height = height % params.grid.y;
    uint y0 = block_y * base_height + min(block_y, extra_height);
    uint y1 = y0 + base_height + uint(block_y < extra_height);"#;
        let hash_body = r#"            hash.x = (hash.x ^ keyed) * 0x01000193u;
            hash.y = rotate_left(
                hash.y + pixel + position * 0x85ebca6bu,
                11u
            ) * 0xc2b2ae35u;
            hash.z = rotate_left(
                hash.z ^ (pixel + position * 0x27d4eb2du),
                13u
            ) * 0x165667b1u;
            hash.w = rotate_left(
                hash.w + (pixel ^ rotate_left(position, 16u)),
                17u
            ) * 0x85ebca6bu;"#;

        assert!(COMPUTE_GLSL.contains(partition));
        assert!(COMPUTE_GLSL.contains(hash_body));
        assert!(!COMPUTE_GLSL.contains("frame_token"));
    }

    #[test]
    fn odd_grid_partition_covers_each_pixel_exactly_once() {
        let (width, height): (u32, u32) = (67, 65);
        let layout = ProbeDigestLayout::new(width, height).unwrap();
        let grid_width = layout.push_constants.grid[0];
        let grid_height = layout.push_constants.grid[1];
        let mut visits = vec![0_u8; (width * height) as usize];

        for block_y in 0..grid_height {
            let (y0, y1) = block_bounds(height, grid_height, block_y);
            for block_x in 0..grid_width {
                let (x0, x1) = block_bounds(width, grid_width, block_x);
                for y in y0..y1 {
                    for x in x0..x1 {
                        visits[(y * width + x) as usize] += 1;
                    }
                }
            }
        }

        assert!(visits.iter().all(|&visits| visits == 1));
    }

    #[test]
    fn one_channel_change_and_horizontal_flip_change_digest() {
        let (width, height): (u32, u32) = (67, 65);
        let pixels: Vec<u32> = (0..width * height)
            .map(|position| 0xff00_0000 ^ position.wrapping_mul(0x0001_0203))
            .collect();
        let original = reference_digest(&pixels, width, height);

        let (changed_x, changed_y) = (10, 20);
        let mut changed = pixels.clone();
        changed[(changed_y * width + changed_x) as usize] ^= 0x0000_0100;
        let changed = reference_digest(&changed, width, height);
        let block = block_for_pixel(width, height, changed_x, changed_y);
        let range = 4 + block * 4..4 + (block + 1) * 4;
        assert!(
            original[range.clone()]
                .iter()
                .zip(&changed[range])
                .all(|(before, after)| before != after)
        );

        let mut flipped = pixels.clone();
        for row in flipped.chunks_exact_mut(width as usize) {
            row.reverse();
        }
        assert_ne!(reference_digest(&flipped, width, height), original);
    }

    #[test]
    fn digest_is_deterministic_and_has_no_frame_token_input() {
        let digest: fn(&[u32], u32, u32) -> Vec<u32> = reference_digest;
        let pixels = [0x1122_3344, 0x5566_7788, 0x99aa_bbcc];
        assert_eq!(digest(&pixels, 3, 1), digest(&pixels, 3, 1));
    }

    #[test]
    fn digest_barriers_cover_the_exact_buffers_and_accesses() {
        let input = vk::Buffer::from_raw(11);
        let before = transfer_to_compute_barrier(input, 4096);
        assert_eq!(before.buffer, input);
        assert_eq!(before.size, 4096);
        assert_eq!(before.src_stage_mask, vk::PipelineStageFlags2::COPY);
        assert_eq!(before.src_access_mask, vk::AccessFlags2::TRANSFER_WRITE);
        assert_eq!(
            before.dst_stage_mask,
            vk::PipelineStageFlags2::COMPUTE_SHADER
        );
        assert_eq!(
            before.dst_access_mask,
            vk::AccessFlags2::SHADER_STORAGE_READ
        );

        let output = vk::Buffer::from_raw(12);
        let after = compute_to_host_barrier(output, 256);
        assert_eq!(after.buffer, output);
        assert_eq!(after.size, 256);
        assert_eq!(
            after.src_stage_mask,
            vk::PipelineStageFlags2::COMPUTE_SHADER
        );
        assert_eq!(
            after.src_access_mask,
            vk::AccessFlags2::SHADER_STORAGE_WRITE
        );
        assert_eq!(after.dst_stage_mask, vk::PipelineStageFlags2::HOST);
        assert_eq!(after.dst_access_mask, vk::AccessFlags2::HOST_READ);
    }

    #[test]
    fn host_output_memory_prefers_cached_even_when_noncoherent() {
        let mut properties = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            ..Default::default()
        };
        properties.memory_types[0].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        properties.memory_types[1].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED;
        properties.memory_types[2].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;

        assert_eq!(
            pick_buffer_memory_type(&properties, 0b111, BufferMemoryKind::HostCached),
            Some(1)
        );
        assert_eq!(
            pick_buffer_memory_type(&properties, 0b111, BufferMemoryKind::DeviceLocal),
            Some(2)
        );
    }

    #[test]
    fn embedded_compute_shader_is_spirv() {
        assert!(COMPUTE_SPV.len() >= 4);
        assert_eq!(
            u32::from_le_bytes(COMPUTE_SPV[..4].try_into().unwrap()),
            0x0723_0203
        );
    }
}
