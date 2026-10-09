use super::*;

/// Usage of every upload arena block (#177): instance/vertex data for the
/// glyph and trapezoid pipelines, and the source of glyph-atlas uploads.
/// One block serves both; both are core usages with no feature
/// requirement, and the memory type is still chosen from the buffer's own
/// requirements (`HOST_VISIBLE | HOST_COHERENT`, so CPU writes need no
/// flush and `nonCoherentAtomSize` does not apply).
const UPLOAD_ARENA_USAGE: vk::BufferUsageFlags = vk::BufferUsageFlags::from_raw(
    vk::BufferUsageFlags::VERTEX_BUFFER.as_raw() | vk::BufferUsageFlags::TRANSFER_SRC.as_raw(),
);

/// Offset alignment of vertex/instance data in the upload arena. Every
/// attribute of the glyph-instance and trapezoid vertex layouts has 32-bit
/// components (`R32_*` / `R32G32_*`), and Vulkan requires each attribute's
/// address to be a multiple of its component size ("Vertex Input Address
/// Calculation"); `vkCmdBindVertexBuffers` itself adds no offset alignment
/// rule. 16 covers that with room for a future 64-bit or vec4 attribute,
/// for at most 15 bytes of padding per request.
pub(super) const UPLOAD_VERTEX_ALIGN: u64 = 16;

/// Upper bound on the `optimalBufferCopyOffsetAlignment` honoured for
/// glyph-atlas uploads. The limit is a performance hint; a driver
/// reporting more than this would pad every few-hundred-byte glyph by
/// more than the glyph itself.
pub(super) const UPLOAD_COPY_ALIGN_MAX: u64 = 256;

/// Offset alignment for glyph-atlas upload sources in the upload arena.
/// `vkCmdCopyBufferToImage` requires `bufferOffset` to be a multiple of 4
/// and of the texel block size (1 byte for the R8 atlas);
/// `optimal_copy_align` (`optimalBufferCopyOffsetAlignment`) is honoured
/// on top, up to [`UPLOAD_COPY_ALIGN_MAX`].
pub(super) fn upload_copy_align(optimal_copy_align: u64) -> u64 {
    optimal_copy_align
        .clamp(4, UPLOAD_COPY_ALIGN_MAX)
        .next_power_of_two()
}

impl StagingBuffer {
    /// Transfer staging, counted under churn `class`.
    fn new(
        vk: Arc<VkContext>,
        size: u64,
        class: crate::kms::vk::mem_accounting::ChurnClass,
    ) -> Result<Self, vk::Result> {
        Self::new_with_usage(
            vk,
            size,
            vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
            class,
        )
    }

    /// Stage 3e.2: variant with explicit usage flags. The trap
    /// path needs `VERTEX_BUFFER` usage on its instance-data
    /// upload buffer (cmd_bind_vertex_buffers requires that bit).
    ///
    /// Upload/general path: prefers plain `HOST_VISIBLE | HOST_COHERENT`
    /// (write-combined is fine — the CPU only *writes* here).
    ///
    /// `class` names the site for the `vram churn` counters (#177): these
    /// buffers live one frame, so only allocation rates show them.
    pub(super) fn new_with_usage(
        vk: Arc<VkContext>,
        size: u64,
        usage: vk::BufferUsageFlags,
        class: crate::kms::vk::mem_accounting::ChurnClass,
    ) -> Result<Self, vk::Result> {
        Self::new_internal(vk, size, usage, false, class)
    }

    /// Readback-optimized staging: prefers a `HOST_CACHED` memory type so
    /// CPU *reads* of the mapped buffer run at cached-RAM speed instead of
    /// write-combined/uncached speed. On discrete GPUs `HOST_COHERENT` is
    /// typically write-combined, where reading back a 2560×1440 GetImage
    /// crawls at ~160 MB/s (~50–90 ms); a cached type makes it near-memcpy.
    /// Falls back to plain `HOST_COHERENT` when no cached type is available
    /// (e.g. some software ICDs). See `RenderEngine::get_image` and
    /// project_cinnamon_nvidia_chop_shm_getimage.
    pub(crate) fn new_for_readback(vk: Arc<VkContext>, size: u64) -> Result<Self, vk::Result> {
        Self::new_internal(
            vk,
            size,
            vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
            true,
            crate::kms::vk::mem_accounting::ChurnClass::Readback,
        )
    }

    fn new_internal(
        vk: Arc<VkContext>,
        size: u64,
        usage: vk::BufferUsageFlags,
        readback: bool,
        class: crate::kms::vk::mem_accounting::ChurnClass,
    ) -> Result<Self, vk::Result> {
        let buf_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { vk.device.create_buffer(&buf_info, None)? };
        let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(buffer) };
        let mem_props = unsafe {
            vk.instance
                .get_physical_device_memory_properties(vk.physical_device)
        };
        let Some((mt, coherent)) =
            Self::pick_memory_type(&mem_props, mem_reqs.memory_type_bits, readback)
        else {
            unsafe { vk.device.destroy_buffer(buffer, None) };
            return Err(vk::Result::ERROR_FEATURE_NOT_PRESENT);
        };
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.size)
            .memory_type_index(mt);
        let memory = match crate::kms::vk::mem_accounting::allocate_memory_as(
            &vk.device,
            &alloc_info,
            crate::kms::vk::mem_accounting::MemCategory::Staging,
            class,
            &mem_props,
        ) {
            Ok(m) => m,
            Err(e) => {
                unsafe { vk.device.destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        if let Err(e) = unsafe { vk.device.bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                vk.device.destroy_buffer(buffer, None);
            }
            return Err(e);
        }
        let mapped_raw = match unsafe {
            vk.device
                .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
        } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    crate::kms::vk::mem_accounting::free_memory(&vk.device, memory);
                    vk.device.destroy_buffer(buffer, None);
                }
                return Err(e);
            }
        };
        let mapped = NonNull::new(mapped_raw.cast::<u8>()).expect("vkMapMemory non-null");
        Ok(Self {
            vk,
            buffer,
            memory,
            mapped,
            size,
            coherent,
            from_pool: false,
        })
    }

    /// Choose a memory type for the staging buffer, returning
    /// `(memory_type_index, is_host_coherent)`.
    ///
    /// Upload (`readback == false`): plain `HOST_VISIBLE | HOST_COHERENT`,
    /// the historical behaviour (CPU only writes; write-combined is fine).
    ///
    /// Readback (`readback == true`): prefer cached types so CPU reads are
    /// fast, in descending order —
    /// 1. `HOST_VISIBLE | HOST_CACHED | HOST_COHERENT` (fast reads, no
    ///    manual invalidate),
    /// 2. `HOST_VISIBLE | HOST_CACHED` (fast reads, needs invalidate),
    /// 3. `HOST_VISIBLE | HOST_COHERENT` (write-combined fallback; correct
    ///    but slow to read — only when nothing cached is offered).
    pub(super) fn pick_memory_type(
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        type_bits: u32,
        readback: bool,
    ) -> Option<(u32, bool)> {
        use vk::MemoryPropertyFlags as F;
        let host = F::HOST_VISIBLE;
        let tiers: &[F] = if readback {
            &[
                host | F::HOST_CACHED | F::HOST_COHERENT,
                host | F::HOST_CACHED,
                host | F::HOST_COHERENT,
            ]
        } else {
            &[host | F::HOST_COHERENT]
        };
        for &want in tiers {
            if let Some(i) = (0..mem_props.memory_type_count).find(|&i| {
                type_bits & (1 << i) != 0
                    && mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(want)
            }) {
                let coherent = mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(F::HOST_COHERENT);
                return Some((i, coherent));
            }
        }
        None
    }

    /// Make the GPU's writes visible to CPU reads of `mapped`. No-op when
    /// the backing memory is `HOST_COHERENT`; otherwise issues
    /// `vkInvalidateMappedMemoryRanges` over the whole allocation. Call
    /// AFTER the readback fence has signalled and BEFORE reading `mapped`.
    pub(crate) fn invalidate_for_read(&self) -> Result<(), vk::Result> {
        if self.coherent {
            return Ok(());
        }
        let range = vk::MappedMemoryRange::default()
            .memory(self.memory)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        unsafe { self.vk.device.invalidate_mapped_memory_ranges(&[range]) }
    }
}

impl StagingBuffer {
    pub(crate) fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    pub(crate) fn mapped(&self) -> NonNull<u8> {
        self.mapped
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn vk(&self) -> &Arc<VkContext> {
        &self.vk
    }
}

impl Drop for StagingBuffer {
    fn drop(&mut self) {
        unsafe {
            self.vk.device.unmap_memory(self.memory);
            self.vk.device.destroy_buffer(self.buffer, None);
            crate::kms::vk::mem_accounting::free_memory(&self.vk.device, self.memory);
        }
    }
}

/// Max buffers kept per exact-size bucket.
const STAGING_POOL_BUCKET_CAP: usize = 16;
/// Total bytes cap across all buckets (~64 MiB). Beyond this, returns drop.
const STAGING_POOL_TOTAL_BYTES_CAP: u64 = 64 * 1024 * 1024;

impl StagingPool {
    /// Reuse a same-size buffer, or allocate a fresh one. The returned buffer
    /// is flagged `from_pool` so retire routes it back here.
    pub(super) fn acquire(
        &mut self,
        vk: &Arc<VkContext>,
        size: u64,
    ) -> Result<StagingBuffer, vk::Result> {
        if let Some(buf) = self.buckets.get_mut(&size).and_then(Vec::pop) {
            self.pooled_bytes = self.pooled_bytes.saturating_sub(buf.size);
            self.hits += 1;
            crate::kms::vk::mem_accounting::note_staging_pool(
                crate::kms::vk::mem_accounting::PoolEvent::Hit,
            );
            return Ok(buf);
        }
        self.misses += 1;
        crate::kms::vk::mem_accounting::note_staging_pool(
            crate::kms::vk::mem_accounting::PoolEvent::Miss,
        );
        let mut buf = StagingBuffer::new(
            Arc::clone(vk),
            size,
            crate::kms::vk::mem_accounting::ChurnClass::StagingPool,
        )?;
        buf.from_pool = true;
        Ok(buf)
    }

    /// Return a retired `from_pool` buffer for reuse, or drop it (destroy) if
    /// the bucket/byte caps are exceeded. Caller guarantees `buf.from_pool`.
    pub(super) fn release(&mut self, buf: StagingBuffer) {
        let bucket = self.buckets.entry(buf.size).or_default();
        if bucket.len() >= STAGING_POOL_BUCKET_CAP
            || self.pooled_bytes.saturating_add(buf.size) > STAGING_POOL_TOTAL_BYTES_CAP
        {
            self.rejected += 1;
            crate::kms::vk::mem_accounting::note_staging_pool(
                crate::kms::vk::mem_accounting::PoolEvent::Dropped,
            );
            return; // buf drops → StagingBuffer::Drop destroys it
        }
        self.pooled_bytes = self.pooled_bytes.saturating_add(buf.size);
        self.returned += 1;
        crate::kms::vk::mem_accounting::note_staging_pool(
            crate::kms::vk::mem_accounting::PoolEvent::Kept,
        );
        bucket.push(buf);
    }

    /// Destroy every pooled buffer. Call at shutdown after the queue is idle.
    /// Logs a lifetime summary so the pool's effectiveness is measurable on HW
    /// (high `hits` vs `misses` ⇒ the per-upload alloc churn was eliminated).
    pub(super) fn drain(&mut self) {
        log::info!(
            "staging pool: hits={} misses={} returned={} rejected={} buckets={} pooled_bytes={}",
            self.hits,
            self.misses,
            self.returned,
            self.rejected,
            self.buckets.len(),
            self.pooled_bytes,
        );
        self.buckets.clear(); // StagingBuffer::Drop frees each
        self.pooled_bytes = 0;
    }
}

impl RenderEngineInner {
    /// #177: copy one request's upload data into the open frame's upload
    /// arena and pin it, returning the pin its recorded op replays from.
    /// `align` is the offset alignment the data's use needs
    /// ([`UPLOAD_VERTEX_ALIGN`] for instance/vertex data,
    /// `self.upload_copy_align` for a buffer→image copy source).
    ///
    /// The bytes land in a block the open frame owns: the frame's current
    /// block, or a block chained onto it (from the arena's idle list, else
    /// freshly allocated). A request larger than a block gets a dedicated
    /// allocation, counted under `churn`. The blocks travel with the
    /// frame's pin set and return to the arena only once its fence has
    /// signalled (the retire walk in [`RenderEngine::poll_retired`]), so
    /// the slice's bytes stay untouched until the GPU has read them.
    ///
    /// Must be called with a frame open, and the returned pin belongs to
    /// that frame: the frame must not close between this call and
    /// recording the op that uses the pin.
    ///
    /// # Errors
    ///
    /// The `vk::Result` of a failed block allocation. The frame's pins and
    /// blocks are then unchanged.
    pub(super) fn upload_to_frame(
        &mut self,
        data: &[u8],
        align: u64,
        churn: crate::kms::vk::mem_accounting::ChurnClass,
    ) -> Result<crate::kms::render::frame_builder::PinnedUploadIdx, vk::Result> {
        use crate::kms::render::upload_arena::{BlockKind, Placement};
        let vk = Arc::clone(&self.vk);
        let open = self
            .frame_builder
            .open
            .as_mut()
            .expect("upload_to_frame: no open frame");
        let size = data.len() as u64;
        let sub = self
            .upload_arena
            .alloc(&mut open.pins.uploads, size, align, |bytes, kind| {
                let class = match kind {
                    BlockKind::Shared => crate::kms::vk::mem_accounting::ChurnClass::UploadArena,
                    BlockKind::Dedicated => churn,
                };
                StagingBuffer::new_with_usage(vk, bytes, UPLOAD_ARENA_USAGE, class)
            })?;
        crate::kms::vk::mem_accounting::note_upload_arena_request(
            size,
            matches!(sub.placement, Placement::Dedicated(_)),
        );
        crate::kms::vk::mem_accounting::set_upload_arena_idle_blocks(self.upload_arena.idle_len());
        let block = open.pins.uploads.block(sub.placement);
        debug_assert!(sub.offset + size <= block.size);
        let offset = usize::try_from(sub.offset).expect("block offset fits usize");
        // SAFETY: `block` is mapped HOST_COHERENT for `block.size` bytes and
        // `[sub.offset, sub.offset + size)` lies inside it; the arena hands
        // each byte range of a frame's blocks out once, and no submitted
        // work reads this block (it is new, or came off the idle list after
        // its previous frame retired).
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                block.mapped.as_ptr().add(offset),
                data.len(),
            );
        }
        let slice = crate::kms::render::frame_builder::UploadSlice {
            buffer: block.buffer,
            offset: sub.offset,
        };
        Ok(open.pins.pin_upload(slice))
    }
}
