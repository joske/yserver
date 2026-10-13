//! GPU-side image comparison for the non-composited damage audit.
//!
//! The caller copies candidate/reference images into the two device-local
//! input buffers, then this pipeline emits one compact four-word summary per
//! tile: mismatch count, first differing pixel index, candidate word,
//! reference word.

use std::{ptr::NonNull, sync::Arc};

use ash::vk;

use super::device::VkContext;

const COMPUTE_SPV: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/damage_audit_compare.comp.spv"));

const MAX_GRID_WIDTH: u32 = 64;
const MAX_GRID_HEIGHT: u32 = 64;
const LOCAL_SIZE_X: u32 = 64;
const SUMMARY_WORDS_PER_TILE: u64 = 4;
const BYTES_PER_PIXEL: u64 = 4;
const BYTES_PER_WORD: u64 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DamageAuditComparePushConstants {
    extent: [u32; 2],
    grid: [u32; 2],
}

const _: () = assert!(std::mem::size_of::<DamageAuditComparePushConstants>() == 16);
const _: () = assert!(std::mem::offset_of!(DamageAuditComparePushConstants, grid) == 8);

impl DamageAuditComparePushConstants {
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
struct DamageAuditCompareLayout {
    push_constants: DamageAuditComparePushConstants,
    input_bytes: u64,
    summary_words: usize,
    summary_bytes: u64,
    dispatch_x: u32,
}

impl DamageAuditCompareLayout {
    fn new(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }

        let input_bytes = u64::from(width)
            .checked_mul(u64::from(height))?
            .checked_mul(BYTES_PER_PIXEL)?;
        let grid_width = width.min(MAX_GRID_WIDTH);
        let grid_height = height.min(MAX_GRID_HEIGHT);
        let tile_count = u64::from(grid_width).checked_mul(u64::from(grid_height))?;
        let summary_words_u64 = tile_count.checked_mul(SUMMARY_WORDS_PER_TILE)?;
        let summary_words = usize::try_from(summary_words_u64).ok()?;
        let summary_bytes = summary_words_u64.checked_mul(BYTES_PER_WORD)?;
        let tile_count_u32 = u32::try_from(tile_count).ok()?;
        let dispatch_x = tile_count_u32.div_ceil(LOCAL_SIZE_X);

        Some(Self {
            push_constants: DamageAuditComparePushConstants {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageAuditTileSummary {
    pub tile_id: u32,
    pub mismatch_count: u32,
    pub first_pixel_index: u32,
    pub candidate: u32,
    pub reference: u32,
}

pub struct DamageAuditComparePipeline {
    vk: Arc<VkContext>,
    layout: DamageAuditCompareLayout,
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    candidate_buffer: vk::Buffer,
    candidate_memory: vk::DeviceMemory,
    reference_buffer: vk::Buffer,
    reference_memory: vk::DeviceMemory,
    output_buffer: vk::Buffer,
    output_memory: vk::DeviceMemory,
    output_mapped: NonNull<u32>,
    output_coherent: bool,
}

impl DamageAuditComparePipeline {
    #[must_use]
    pub fn is_supported(vk: &VkContext, width: u32, height: u32) -> bool {
        let Some(layout) = DamageAuditCompareLayout::new(width, height) else {
            return false;
        };
        vk.graphics_queue_supports_compute()
            && layout.input_bytes <= vk.max_storage_buffer_range()
            && layout.summary_bytes <= vk.max_storage_buffer_range()
    }

    pub fn new(vk: Arc<VkContext>, width: u32, height: u32) -> Result<Self, vk::Result> {
        let layout = DamageAuditCompareLayout::new(width, height)
            .ok_or(vk::Result::ERROR_INITIALIZATION_FAILED)?;
        if !Self::is_supported(&vk, width, height) {
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        }

        let mut construction = DamageAuditCompareConstruction::new(&vk);
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
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
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
            .size(std::mem::size_of::<DamageAuditComparePushConstants>() as u32)];
        let pipeline_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&descriptor_layouts)
            .push_constant_ranges(&push_constant_ranges);
        construction.handles.pipeline_layout = unsafe {
            vk.device
                .create_pipeline_layout(&pipeline_layout_info, None)?
        };
        construction.handles.pipeline =
            build_pipeline(&vk.device, construction.handles.pipeline_layout)?;

        let candidate = allocate_buffer(
            &vk,
            layout.input_bytes,
            vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::STORAGE_BUFFER,
            BufferMemoryKind::DeviceLocal,
        )?;
        construction.handles.candidate_buffer = candidate.buffer;
        construction.handles.candidate_memory = candidate.memory;

        let reference = allocate_buffer(
            &vk,
            layout.input_bytes,
            vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::STORAGE_BUFFER,
            BufferMemoryKind::DeviceLocal,
        )?;
        construction.handles.reference_buffer = reference.buffer;
        construction.handles.reference_memory = reference.memory;

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
            Some(NonNull::new(mapped.cast::<u32>()).expect("vkMapMemory returned null"));

        let pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::STORAGE_BUFFER,
            descriptor_count: 3,
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

        let candidate_info = [vk::DescriptorBufferInfo::default()
            .buffer(candidate.buffer)
            .offset(0)
            .range(layout.input_bytes)];
        let reference_info = [vk::DescriptorBufferInfo::default()
            .buffer(reference.buffer)
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
                .buffer_info(&candidate_info),
            vk::WriteDescriptorSet::default()
                .dst_set(construction.handles.descriptor_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&reference_info),
            vk::WriteDescriptorSet::default()
                .dst_set(construction.handles.descriptor_set)
                .dst_binding(2)
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
            candidate_buffer: handles.candidate_buffer,
            candidate_memory: handles.candidate_memory,
            reference_buffer: handles.reference_buffer,
            reference_memory: handles.reference_memory,
            output_buffer: handles.output_buffer,
            output_memory: handles.output_memory,
            output_mapped: handles
                .output_mapped
                .expect("successful compare construction maps its output"),
            output_coherent: handles.output_coherent,
        })
    }

    #[must_use]
    pub const fn candidate_buffer(&self) -> vk::Buffer {
        self.candidate_buffer
    }

    #[must_use]
    pub const fn reference_buffer(&self) -> vk::Buffer {
        self.reference_buffer
    }

    #[must_use]
    pub const fn grid_width(&self) -> u32 {
        self.layout.push_constants.grid[0]
    }

    #[must_use]
    pub const fn grid_height(&self) -> u32 {
        self.layout.push_constants.grid[1]
    }

    pub fn record_after_transfers(&self, command_buffer: vk::CommandBuffer) {
        let input_barriers = [
            transfer_to_compute_barrier(self.candidate_buffer, self.layout.input_bytes),
            transfer_to_compute_barrier(self.reference_buffer, self.layout.input_bytes),
        ];
        let output_barriers = [compute_to_host_barrier(
            self.output_buffer,
            self.layout.summary_bytes,
        )];
        unsafe {
            self.vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().buffer_memory_barriers(&input_barriers),
            );
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
            self.vk.device.cmd_pipeline_barrier2(
                command_buffer,
                &vk::DependencyInfo::default().buffer_memory_barriers(&output_barriers),
            );
        }
    }

    pub fn read_summary(&self) -> Result<Vec<DamageAuditTileSummary>, vk::Result> {
        if !self.output_coherent {
            let ranges = [vk::MappedMemoryRange::default()
                .memory(self.output_memory)
                .offset(0)
                .size(vk::WHOLE_SIZE)];
            unsafe {
                self.vk.device.invalidate_mapped_memory_ranges(&ranges)?;
            }
        }

        let words = unsafe {
            std::slice::from_raw_parts(self.output_mapped.as_ptr(), self.layout.summary_words)
        };
        let mut tiles = Vec::with_capacity(words.len() / 4);
        for (tile_id, chunk) in words.chunks_exact(4).enumerate() {
            tiles.push(DamageAuditTileSummary {
                tile_id: u32::try_from(tile_id).unwrap_or(u32::MAX),
                mismatch_count: chunk[0],
                first_pixel_index: chunk[1],
                candidate: chunk[2],
                reference: chunk[3],
            });
        }
        Ok(tiles)
    }
}

impl Drop for DamageAuditComparePipeline {
    fn drop(&mut self) {
        if self.vk.requires_drop_device_idle() {
            let wait = unsafe { self.vk.device.device_wait_idle() };
            if !matches!(wait, Ok(()) | Err(vk::Result::ERROR_DEVICE_LOST)) {
                log::warn!(
                    "damage audit compare: vkDeviceWaitIdle failed during teardown: {wait:?}; \
                     leaking uncertain compare resources"
                );
                std::mem::forget(Arc::clone(&self.vk));
                return;
            }
        }
        unsafe {
            self.vk.device.unmap_memory(self.output_memory);
            destroy_handles(
                &self.vk.device,
                &mut DamageAuditCompareHandles {
                    descriptor_set_layout: self.descriptor_set_layout,
                    descriptor_pool: self.descriptor_pool,
                    descriptor_set: self.descriptor_set,
                    pipeline_layout: self.pipeline_layout,
                    pipeline: self.pipeline,
                    candidate_buffer: self.candidate_buffer,
                    candidate_memory: self.candidate_memory,
                    reference_buffer: self.reference_buffer,
                    reference_memory: self.reference_memory,
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
    );
    let Some(memory_type_index) = memory_type_index else {
        unsafe { vk.device.destroy_buffer(buffer, None) };
        return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
    };
    let selected_properties =
        memory_properties.memory_types[memory_type_index as usize].property_flags;

    let allocation_info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_type_index);
    let memory = match crate::kms::vk::mem_accounting::allocate_memory(
        &vk.device,
        &allocation_info,
        crate::kms::vk::mem_accounting::MemCategory::Other,
        &memory_properties,
    ) {
        Ok(memory) => memory,
        Err(error) => {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(error);
        }
    };
    if let Err(error) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
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
struct DamageAuditCompareHandles {
    descriptor_set_layout: vk::DescriptorSetLayout,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    candidate_buffer: vk::Buffer,
    candidate_memory: vk::DeviceMemory,
    reference_buffer: vk::Buffer,
    reference_memory: vk::DeviceMemory,
    output_buffer: vk::Buffer,
    output_memory: vk::DeviceMemory,
    output_mapped: Option<NonNull<u32>>,
    output_coherent: bool,
}

struct DamageAuditCompareConstruction<'a> {
    vk: &'a VkContext,
    handles: DamageAuditCompareHandles,
    armed: bool,
}

impl<'a> DamageAuditCompareConstruction<'a> {
    fn new(vk: &'a VkContext) -> Self {
        Self {
            vk,
            handles: DamageAuditCompareHandles::default(),
            armed: true,
        }
    }

    fn finish(mut self) -> DamageAuditCompareHandles {
        self.armed = false;
        self.handles
    }
}

impl Drop for DamageAuditCompareConstruction<'_> {
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

unsafe fn destroy_handles(device: &ash::Device, handles: &mut DamageAuditCompareHandles) {
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
    if handles.candidate_buffer != vk::Buffer::null() {
        unsafe { device.destroy_buffer(handles.candidate_buffer, None) };
        handles.candidate_buffer = vk::Buffer::null();
    }
    if handles.candidate_memory != vk::DeviceMemory::null() {
        unsafe { crate::kms::vk::mem_accounting::free_memory(device, handles.candidate_memory) };
        handles.candidate_memory = vk::DeviceMemory::null();
    }
    if handles.reference_buffer != vk::Buffer::null() {
        unsafe { device.destroy_buffer(handles.reference_buffer, None) };
        handles.reference_buffer = vk::Buffer::null();
    }
    if handles.reference_memory != vk::DeviceMemory::null() {
        unsafe { crate::kms::vk::mem_accounting::free_memory(device, handles.reference_memory) };
        handles.reference_memory = vk::DeviceMemory::null();
    }
    if handles.output_buffer != vk::Buffer::null() {
        unsafe { device.destroy_buffer(handles.output_buffer, None) };
        handles.output_buffer = vk::Buffer::null();
    }
    if handles.output_memory != vk::DeviceMemory::null() {
        unsafe { crate::kms::vk::mem_accounting::free_memory(device, handles.output_memory) };
        handles.output_memory = vk::DeviceMemory::null();
    }
}
